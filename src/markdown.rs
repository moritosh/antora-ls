//! AsciiDoc → Markdown for an in-editor preview through the editor's Markdown preview.
//!
//! Covers the constructs Antora pages commonly use. Antora resource references are resolved
//! with the catalog: xrefs link to the target's source file, includes are inlined (honoring
//! `tag(s)=`, `lines=` and `leveloffset=`), and images point at the image file. Anything
//! Markdown can't express is approximated (admonitions and sidebars become quotes).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::{Captures, Regex};

use crate::catalog::{Catalog, MacroKind, Resolved};
use crate::refs::Texts;
use crate::scan;

const MAX_INCLUDE_DEPTH: usize = 8;

/// `image_base`: directory the Markdown file will live in. Image paths are written relative to
/// it, because Markdown previews treat a leading `/` as the workspace root.
pub fn convert(cat: &Catalog, file: &Path, text: &str, texts: Texts, image_base: Option<&Path>) -> Vec<String> {
    let ctx = cat.locate(file);
    let attrs = cat.attributes_for(ctx.as_ref()).cloned().unwrap_or_default();
    let mut conv = Converter { cat, texts, attrs, image_base };
    for (k, v) in [("empty", ""), ("sp", " "), ("nbsp", "\u{a0}"), ("zwsp", "\u{200b}"), ("plus", "+"), ("startsb", "["), ("endsb", "]"), ("vbar", "|"), ("caret", "^"), ("asterisk", "*"), ("tilde", "~"), ("backtick", "`"), ("amp", "&"), ("lt", "<"), ("gt", ">")] {
        conv.attrs.entry(k.into()).or_insert_with(|| v.into());
    }
    // Intrinsic attributes Antora sets on every page.
    if let Some(c) = &ctx {
        let cv = &cat.components[c.cv];
        let stem = Path::new(&c.relative).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        for (k, v) in [
            ("page-component-name", cv.name.clone()),
            ("page-component-version", cv.version.clone()),
            ("page-version", cv.version.clone()),
            ("page-component-display-version", if cv.version.is_empty() { "default".into() } else { cv.version.clone() }),
            ("page-component-title", cv.title.clone().unwrap_or_else(|| cv.name.clone())),
            ("page-module", c.module.clone()),
            ("page-relative-src-path", c.relative.clone()),
            ("docname", stem),
        ] {
            conv.attrs.insert(k.into(), v);
        }
    }
    let mut lines = Vec::new();
    conv.expand(file, text, 0, &mut lines);
    let (doc_title, body) = conv.header(&lines);
    let mut out = Vec::new();
    if let Some(title) = doc_title {
        out.push(format!("# {}", conv.inline(file, &title)));
        out.push(String::new());
    }
    out.extend(conv.blocks(body));
    tidy(out)
}

struct Converter<'a> {
    cat: &'a Catalog,
    texts: Texts<'a>,
    attrs: BTreeMap<String, String>,
    image_base: Option<&'a Path>,
}

/// A source line tagged with the file it came from (for resolving relative references).
#[derive(Clone)]
struct Line {
    text: String,
    file: std::path::PathBuf,
}

#[derive(Default, Clone)]
struct BlockAttrs {
    style: Option<String>,
    positional: Vec<String>,
    named: BTreeMap<String, String>,
    options: Vec<String>,
}

impl BlockAttrs {
    fn parse(inner: &str) -> Self {
        let mut a = BlockAttrs::default();
        for (i, part) in split_attrs(inner).into_iter().enumerate() {
            let part = part.trim();
            if let Some((k, v)) = part.split_once('=') {
                let v = v.trim().trim_matches('"').to_string();
                if k.trim() == "options" || k.trim() == "opts" {
                    a.options.extend(v.split(',').map(|o| o.trim().to_string()));
                }
                a.named.insert(k.trim().to_string(), v);
            } else if i == 0 {
                // `source%linenums#id.role`: style, then shorthand options/id/roles
                let mut style = String::new();
                let mut rest = part;
                while let Some(c) = rest.chars().next() {
                    if "%#.".contains(c) {
                        break;
                    }
                    style.push(c);
                    rest = &rest[c.len_utf8()..];
                }
                for opt in rest.split('%').skip(1) {
                    a.options.push(opt.split(['#', '.']).next().unwrap_or("").to_string());
                }
                if !style.is_empty() {
                    a.style = Some(style.clone());
                }
                a.positional.push(style);
            } else {
                a.positional.push(part.trim_matches('"').to_string());
            }
        }
        a
    }

    fn has_option(&self, o: &str) -> bool {
        self.options.iter().any(|x| x == o)
    }
}

/// Split `a,"b,c",d` on commas outside quotes.
fn split_attrs(s: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                out.last_mut().unwrap().push(c);
            }
            ',' if !quoted => out.push(String::new()),
            _ => out.last_mut().unwrap().push(c),
        }
    }
    out
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap())
}

macro_rules! regex {
    ($p:expr) => {{
        static R: OnceLock<Regex> = OnceLock::new();
        re(&R, $p)
    }};
}

const ADMONITIONS: [&str; 5] = ["NOTE", "TIP", "IMPORTANT", "CAUTION", "WARNING"];

impl Converter<'_> {
    // --- preprocessing: comments, conditionals, attribute entries, includes -----------------

    fn expand(&mut self, file: &Path, text: &str, depth: usize, out: &mut Vec<Line>) {
        let mut in_comment = false;
        let mut skipping: Vec<bool> = Vec::new(); // conditional stack: true = inactive
        let active = |s: &Vec<bool>| !s.iter().any(|&x| x);
        for raw in scan::lines(text) {
            let t = raw.trim_end();
            if t.len() >= 4 && t.chars().all(|c| c == '/') {
                in_comment = !in_comment;
                continue;
            }
            if in_comment {
                continue;
            }
            if let Some(c) = regex!(r"^(ifdef|ifndef|ifeval)::([^\[]*)\[(.*)\]$").captures(t) {
                let cond = match &c[1] {
                    "ifeval" => true, // can't evaluate expressions; show the content
                    kind => {
                        let names = &c[2];
                        let defined = |n: &str| self.attrs.contains_key(n.trim());
                        let hit = if names.contains('+') { names.split('+').all(defined) } else { names.split(',').any(defined) };
                        hit == (kind == "ifdef")
                    }
                };
                if !c[3].is_empty() {
                    if cond && active(&skipping) {
                        out.push(Line { text: c[3].to_string(), file: file.to_path_buf() });
                    }
                } else {
                    skipping.push(!cond);
                }
                continue;
            }
            if regex!(r"^endif::[^\[]*\[\]$").is_match(t) {
                skipping.pop();
                continue;
            }
            if !active(&skipping) {
                continue;
            }
            if let Some(c) = regex!(r"^:(!?)([\w][\w-]*)(!?):\s*(.*)$").captures(t) {
                let name = c[2].to_string();
                if !c[1].is_empty() || !c[3].is_empty() {
                    self.attrs.remove(&name);
                } else {
                    let value = self.substitute(c[4].trim_end_matches(" \\"));
                    self.attrs.insert(name, value);
                }
                continue;
            }
            if let Some(c) = regex!(r"^include::([^\[]+)\[(.*)\]$").captures(t) {
                self.include(file, &c[1], &c[2], depth, out);
                continue;
            }
            out.push(Line { text: raw.to_string(), file: file.to_path_buf() });
        }
    }

    fn include(&mut self, file: &Path, target: &str, attrlist: &str, depth: usize, out: &mut Vec<Line>) {
        let target = self.substitute(target);
        let warn = |out: &mut Vec<Line>, msg: String| {
            for text in ["".to_string(), format!("WARNING: {msg}"), "".to_string()] {
                out.push(Line { text, file: file.to_path_buf() });
            }
        };
        if depth >= MAX_INCLUDE_DEPTH {
            return warn(out, format!("Include nesting too deep at `{target}`"));
        }
        let ctx = self.cat.locate(file);
        let path = match self.cat.resolve(ctx.as_ref(), file, &target, MacroKind::Include) {
            Resolved::Found(p) | Resolved::Alias(p) => p,
            _ => return warn(out, format!("Include not found: `{target}`")),
        };
        let Some(text) = (self.texts)(&path) else {
            return warn(out, format!("Can't read `{}`", path.display()));
        };
        let attrs = BlockAttrs::parse(attrlist);
        let mut selected = select_lines(&text, &attrs);
        if let Some(offset) = attrs.named.get("leveloffset").and_then(|o| o.trim_start_matches('+').parse::<i32>().ok()) {
            selected = shift_sections(&selected, offset);
        }
        self.expand(&path, &selected, depth + 1, out);
    }

    fn substitute(&self, s: &str) -> String {
        regex!(r"\\?\{([\w][\w-]*)\}")
            .replace_all(s, |c: &Captures| {
                if c[0].starts_with('\\') {
                    return c[0][1..].to_string();
                }
                self.attrs.get(&c[1]).cloned().unwrap_or_else(|| c[0].to_string())
            })
            .into_owned()
    }

    /// Splits off the document header: returns the title and the body lines.
    fn header<'l>(&self, lines: &'l [Line]) -> (Option<String>, &'l [Line]) {
        let start = lines.iter().position(|l| !l.text.trim().is_empty()).unwrap_or(lines.len());
        let Some(title) = lines.get(start).and_then(|l| l.text.strip_prefix("= ")) else {
            return (None, &lines[start..]);
        };
        // Author and revision lines follow the title until the first blank line.
        let end = lines[start..].iter().position(|l| l.text.trim().is_empty()).map_or(lines.len(), |p| start + p);
        (Some(title.trim().to_string()), &lines[end..])
    }

    // --- blocks ---------------------------------------------------------------------------

    fn blocks(&mut self, lines: &[Line]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut attrs: Option<BlockAttrs> = None;
        let mut title: Option<String> = None;
        let mut lists: Vec<(String, usize)> = Vec::new(); // (marker, content indent)
        let mut attach: Option<usize> = None;
        let mut i = 0;

        while i < lines.len() {
            let line = &lines[i];
            let t = line.text.trim_end();
            let f = line.file.as_path();

            if t.is_empty() {
                out.push(String::new());
                i += 1;
                continue;
            }
            if t.starts_with("//") && !t.starts_with("///") {
                i += 1;
                continue;
            }
            if t == "+" && !lists.is_empty() {
                attach = lists.last().map(|l| l.1);
                i += 1;
                continue;
            }
            if regex!(r"^\[\[[\w:.-]+(,[^\]]*)?\]\]$").is_match(t) || regex!(r"^\[#[\w:.-]+\]$").is_match(t) {
                i += 1;
                continue; // block anchor
            }
            if t.starts_with('[') && t.ends_with(']') && !t.starts_with("[[") {
                attrs = Some(BlockAttrs::parse(&t[1..t.len() - 1]));
                i += 1;
                continue;
            }
            if let Some(rest) = t.strip_prefix('.') {
                if !rest.is_empty() && !rest.starts_with(['.', ' ', '\t']) {
                    title = Some(self.inline(f, rest));
                    i += 1;
                    continue;
                }
            }

            let block_attrs = attrs.take().unwrap_or_default();
            let block_title = title.take();
            let (rendered, consumed, is_list) = self.block(f, lines, i, &block_attrs, block_title, &mut lists);
            if !is_list && attach.is_none() {
                lists.clear();
            }
            let indent = attach.take().map(|n| " ".repeat(n)).unwrap_or_default();
            out.extend(rendered.into_iter().map(|l| if l.is_empty() { l } else { format!("{indent}{l}") }));
            i += consumed;
        }
        out
    }

    /// Renders the block starting at `lines[i]`; returns (markdown lines, lines consumed, is list item).
    fn block(
        &mut self,
        f: &Path,
        lines: &[Line],
        i: usize,
        attrs: &BlockAttrs,
        title: Option<String>,
        lists: &mut Vec<(String, usize)>,
    ) -> (Vec<String>, usize, bool) {
        let t = lines[i].text.trim_end();
        let mut out = Vec::new();
        let style = attrs.style.as_deref().unwrap_or("");

        // Section title (discrete headings render the same)
        if let Some(c) = regex!(r"^(={2,6})\s+(.+)$").captures(t) {
            out.push(format!("{} {}", "#".repeat(c[1].len()), self.inline(f, &c[2])));
            return (out, 1, false);
        }

        // Delimited blocks
        if let Some(kind) = delimiter(t) {
            let end = lines[i + 1..].iter().position(|l| l.text.trim_end() == t).map_or(lines.len(), |p| i + 1 + p);
            let inner = &lines[i + 1..end.min(lines.len())];
            let consumed = end.min(lines.len()) - i + usize::from(end < lines.len());
            out.extend(self.delimited(f, kind, inner, attrs, title));
            return (out, consumed, false);
        }

        let caption = |out: &mut Vec<String>, title: &Option<String>| {
            if let Some(t) = title {
                out.push(format!("**{t}**"));
                out.push(String::new());
            }
        };

        // Block macros
        if let Some(c) = regex!(r"^image::([^\[]+)\[(.*)\]$").captures(t) {
            caption(&mut out, &title);
            out.push(self.image(f, &c[1], &c[2]));
            return (out, 1, false);
        }
        if let Some(c) = regex!(r"^(video|audio)::([^\[]+)\[(.*)\]$").captures(t) {
            let target = self.substitute(&c[2]);
            let url = if c[3].contains("youtube") { format!("https://www.youtube.com/watch?v={target}") } else { target.clone() };
            out.push(format!("[▶ {} {target}]({url})", &c[1]));
            return (out, 1, false);
        }
        if regex!(r"^toc::\[.*\]$").is_match(t) || t == "<<<" {
            return (out, 1, false);
        }
        if t == "'''" || t == "---" || t == "***" || t == "- - -" || t == "* * *" {
            out.push("---".into());
            return (out, 1, false);
        }

        // Lists
        if let Some(c) = regex!(r"^(\*{1,5}|-|\.{1,5})\s+(.*)$").captures(t) {
            let marker = c[1].to_string();
            let depth = match lists.iter().position(|(m, _)| *m == marker) {
                Some(d) => d,
                None => lists.len(),
            };
            lists.truncate(depth);
            let indent: usize = lists.iter().map(|l| l.1).sum::<usize>();
            let ordered = marker.starts_with('.');
            let bullet = if ordered { "1. " } else { "- " };
            lists.push((marker, bullet.len()));
            let (text, consumed) = self.item_text(f, lines, i, &c[2]);
            caption(&mut out, &title);
            let text = regex!(r"^\[([ xX*])\]\s+").replace(&text, |c: &Captures| {
                format!("[{}] ", if c[1].trim().is_empty() { " " } else { "x" })
            });
            out.push(format!("{}{bullet}{text}", " ".repeat(indent)));
            return (out, consumed, true);
        }
        if let Some(c) = regex!(r"^<(\d+|\.)>\s+(.*)$").captures(t) {
            let n = if &c[1] == "." { "1".to_string() } else { c[1].to_string() };
            let (text, consumed) = self.item_text(f, lines, i, &c[2]);
            out.push(format!("{n}. {text}"));
            return (out, consumed, false);
        }
        if let Some(c) = regex!(r"^(\S.*?)(:{2,4}|;;)(?:\s+(.*))?$").captures(t) {
            if !c[1].contains("://") && !t.starts_with("image:") {
                let term = self.inline(f, &c[1]);
                let (text, consumed) = self.item_text(f, lines, i, c.get(3).map_or("", |m| m.as_str()));
                let depth = c[2].len().saturating_sub(2);
                caption(&mut out, &title);
                let item = if text.is_empty() { format!("**{term}**") } else { format!("**{term}** — {text}") };
                lists.truncate(depth);
                lists.push((c[2].to_string(), 2));
                out.push(format!("{}- {item}", "  ".repeat(depth)));
                return (out, consumed, true);
            }
        }

        // Tables written without a preceding attribute line are caught by `delimiter`.

        // Paragraph (possibly styled)
        let literal = t.starts_with([' ', '\t']);
        let end = lines[i..]
            .iter()
            .position(|l| {
                let s = l.text.trim_end();
                s.is_empty() || (!literal && delimiter(s).is_some())
            })
            .map_or(lines.len(), |p| i + p);
        let para: Vec<&str> = lines[i..end].iter().map(|l| l.text.trim_end()).collect();
        let consumed = end - i;

        if literal || style == "literal" || style == "source" || style == "listing" {
            caption(&mut out, &title);
            let lang = if style == "source" { attrs.positional.get(1).cloned().unwrap_or_default() } else { String::new() };
            let min_indent = para.iter().map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
            out.push(format!("```{lang}"));
            out.extend(para.iter().map(|l| l.get(min_indent..).unwrap_or("").to_string()));
            out.push("```".into());
            return (out, consumed, false);
        }

        let admonition = ADMONITIONS.iter().find(|a| style == **a).copied().map(|a| (a, para.join("\n")));
        let admonition = admonition.or_else(|| {
            let c = regex!(r"^(NOTE|TIP|IMPORTANT|CAUTION|WARNING):\s+").captures(para[0])?;
            let label = ADMONITIONS.iter().find(|a| **a == &c[1])?;
            Some((*label, para.join("\n")[c[0].len()..].to_string()))
        });
        if let Some((label, body)) = admonition {
            let text = self.paragraph(f, &body.lines().collect::<Vec<_>>());
            out.push(format!("> **{}:** {}", capitalize(label), text.replace('\n', "\n> ")));
            return (out, consumed, false);
        }
        if style == "quote" || style == "verse" {
            let text = if style == "verse" { para.iter().map(|l| self.inline(f, l)).collect::<Vec<_>>().join("  \n") } else { self.paragraph(f, &para) };
            caption(&mut out, &title);
            out.extend(text.lines().map(|l| format!("> {l}")));
            if let Some(who) = attrs.positional.get(1).filter(|s| !s.is_empty()) {
                out.push(format!("> — {}", self.inline(f, who)));
            }
            return (out, consumed, false);
        }

        caption(&mut out, &title);
        out.push(self.paragraph(f, &para));
        (out, consumed, false)
    }

    /// Text of a list item: its first line plus continuation lines up to a blank line or the next item.
    fn item_text(&self, f: &Path, lines: &[Line], i: usize, first: &str) -> (String, usize) {
        let mut parts = vec![first.to_string()];
        let mut j = i + 1;
        while j < lines.len() {
            let s = lines[j].text.trim();
            if s.is_empty() || s == "+" || delimiter(s).is_some() || s.starts_with('[')
                || regex!(r"^(\*{1,5}|-|\.{1,5})\s+|^<(\d+|\.)>\s+|^\S.*?(:{2,4}|;;)(\s|$)").is_match(s)
            {
                break;
            }
            parts.push(s.to_string());
            j += 1;
        }
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        (self.paragraph(f, &refs), j - i)
    }

    fn paragraph(&self, f: &Path, lines: &[&str]) -> String {
        lines
            .iter()
            .map(|l| match l.strip_suffix(" +") {
                Some(head) => format!("{}  ", self.inline(f, head)),
                None => self.inline(f, l),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn delimited(&mut self, f: &Path, kind: Delim, inner: &[Line], attrs: &BlockAttrs, title: Option<String>) -> Vec<String> {
        let style = attrs.style.as_deref().unwrap_or("");
        let mut out = Vec::new();
        let raw = || inner.iter().map(|l| l.text.clone()).collect::<Vec<_>>();
        match kind {
            Delim::Listing | Delim::Literal => {
                if let Some(t) = &title {
                    out.push(format!("**{t}**"));
                    out.push(String::new());
                }
                let lang = if style == "source" || kind == Delim::Listing && attrs.positional.len() > 1 {
                    attrs.positional.get(1).cloned().unwrap_or_default()
                } else {
                    String::new()
                };
                let mut body = raw();
                if attrs.named.get("subs").is_some_and(|s| s.contains("attributes")) {
                    body = body.iter().map(|l| self.substitute(l)).collect();
                }
                let fence = if body.iter().any(|l| l.contains("```")) { "~~~~" } else { "```" };
                out.push(format!("{fence}{lang}"));
                out.extend(body);
                out.push(fence.into());
            }
            Delim::Pass => out.extend(raw()),
            Delim::Open => out.extend(self.blocks(inner)),
            Delim::Table(sep) => {
                if let Some(t) = &title {
                    out.push(format!("**{t}**"));
                    out.push(String::new());
                }
                out.extend(self.table(f, inner, attrs, sep));
            }
            Delim::Example | Delim::Sidebar | Delim::Quote => {
                let label = ADMONITIONS.iter().find(|a| style == **a).map(|a| capitalize(a));
                let heading = match (label, &title) {
                    (Some(l), Some(t)) => Some(format!("**{l}: {t}**")),
                    (Some(l), None) => Some(format!("**{l}:**")),
                    (None, Some(t)) => Some(format!("**{t}**")),
                    (None, None) => None,
                };
                let body = self.blocks(inner);
                out.extend(heading.into_iter().chain(heading_gap(&body)));
                out.extend(tidy(body));
                if kind == Delim::Quote {
                    if let Some(who) = attrs.positional.get(1).filter(|s| !s.is_empty()) {
                        out.push(String::new());
                        out.push(format!("— {}", self.inline(f, who)));
                    }
                }
                out = out.into_iter().map(|l| if l.is_empty() { ">".into() } else { format!("> {l}") }).collect();
            }
        }
        out
    }

    fn table(&self, f: &Path, inner: &[Line], attrs: &BlockAttrs, sep: char) -> Vec<String> {
        let texts: Vec<&str> = inner.iter().map(|l| l.text.trim_end()).collect();
        let mut cells: Vec<(String, usize)> = Vec::new(); // (content, colspan)
        let mut first_row_cells = 0;
        let mut first_line_done = false;
        let implicit_header = texts.len() > 1 && !texts[0].is_empty() && texts[1].is_empty();
        for line in &texts {
            if line.is_empty() {
                if !cells.is_empty() {
                    first_line_done = true;
                }
                continue;
            }
            let row = split_cells(line, sep);
            if row.is_empty() {
                // continuation of the previous cell
                if let Some(last) = cells.last_mut() {
                    last.0.push(' ');
                    last.0.push_str(line.trim());
                }
                continue;
            }
            if !first_line_done && cells.is_empty() {
                first_row_cells = row.iter().map(|c| c.1).sum();
            }
            first_line_done = true;
            cells.extend(row);
        }
        let cols = attrs.named.get("cols").map(|c| count_cols(c)).filter(|&n| n > 0).unwrap_or(first_row_cells.max(1));

        let mut rows: Vec<Vec<String>> = vec![Vec::new()];
        for (content, span) in cells {
            if rows.last().unwrap().len() >= cols {
                rows.push(Vec::new());
            }
            let row = rows.last_mut().unwrap();
            row.push(self.inline(f, content.trim()).replace('|', "\\|"));
            for _ in 1..span {
                row.push(String::new());
            }
        }
        rows.retain(|r| !r.is_empty());
        let header = !attrs.has_option("noheader") && (attrs.has_option("header") || implicit_header);
        let mut out = Vec::new();
        let fmt = |r: &Vec<String>| {
            let mut r = r.clone();
            r.resize(cols, String::new());
            format!("| {} |", r.join(" | "))
        };
        let (head, body) = if header && !rows.is_empty() { (rows[0].clone(), &rows[1..]) } else { (vec![String::new(); cols], &rows[..]) };
        out.push(fmt(&head));
        out.push(format!("|{}", " --- |".repeat(cols)));
        out.extend(body.iter().map(fmt));
        out
    }

    fn image(&self, f: &Path, target: &str, attrlist: &str) -> String {
        let target = self.substitute(target.trim());
        let attrs = BlockAttrs::parse(attrlist);
        let alt = attrs.positional.first().filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| {
            Path::new(&target).file_stem().map(|s| s.to_string_lossy().replace(['-', '_'], " ")).unwrap_or_default()
        });
        if target.contains("://") {
            return format!("![{alt}]({target})");
        }
        let ctx = self.cat.locate(f);
        match self.cat.resolve(ctx.as_ref(), f, &target, MacroKind::Image) {
            Resolved::Found(p) => match self.image_base {
                Some(base) => format!("![{alt}](<{}>)", crate::refs::relative_path(base, &p)),
                None => format!("![{alt}](<{}>)", p.display()),
            },
            _ => format!("⚠️ image not found: `{target}`"),
        }
    }

    // --- inline ---------------------------------------------------------------------------

    fn inline(&self, f: &Path, s: &str) -> String {
        let s = self.substitute(s);
        // Protect code spans first; their content is literal.
        let mut codes: Vec<String> = Vec::new();
        let s = regex!(r"``(.+?)``|`([^`]+)`").replace_all(&s, |c: &Captures| {
            let body = c.get(1).or(c.get(2)).unwrap().as_str();
            let body = body.strip_prefix('+').and_then(|b| b.strip_suffix('+')).unwrap_or(body);
            codes.push(if body.contains('`') { format!("`` {body} ``") } else { format!("`{body}`") });
            format!("\u{0}{}\u{0}", codes.len() - 1)
        });

        let ctx = self.cat.locate(f);
        let s = regex!(r"xref:([^\s\[\]]+)\[([^\]]*)\]").replace_all(&s, |c: &Captures| {
            let target = &c[1];
            let text = c[2].split(",role=").next().unwrap_or("").trim().to_string();
            match self.cat.resolve(ctx.as_ref(), f, target, MacroKind::Xref) {
                Resolved::Found(p) | Resolved::Alias(p) => {
                    let source = (self.texts)(&p);
                    let text = if text.is_empty() {
                        source.as_deref().and_then(scan::document_title).unwrap_or_else(|| target.to_string())
                    } else {
                        text
                    };
                    // `#L<line>` makes the editor open the target at the anchor.
                    let line = target
                        .split_once('#')
                        .and_then(|(_, frag)| scan::find_anchor(source.as_deref()?, frag))
                        .map(|n| format!("#L{}", n + 1))
                        .unwrap_or_default();
                    format!("[{text}](<{}{line}>)", p.display())
                }
                _ => format!("{} ⚠️", if text.is_empty() { target.to_string() } else { text }),
            }
        });
        let s = regex!(r"<<([^,>]+)(?:,\s*([^>]+))?>>").replace_all(&s, |c: &Captures| {
            c.get(2).map_or_else(|| c[1].trim().to_string(), |m| m.as_str().trim().to_string())
        });
        let s = regex!(r"image:([^:\s\[][^\s\[]*)\[([^\]]*)\]").replace_all(&s, |c: &Captures| self.image(f, &c[1], &c[2]));
        let s = regex!(r"(?:link:)?((?:https?|ftp|file|mailto|irc):[^\s\[\]<>]+)\[([^\]]*)\]").replace_all(&s, |c: &Captures| {
            let text = c[2].split(',').next().unwrap_or("").trim_end_matches('^').trim();
            let text = if text.is_empty() || text.contains('=') { c[1].trim_start_matches("mailto:").to_string() } else { text.to_string() };
            format!("[{text}]({})", &c[1])
        });
        let s = regex!(r"link:([^\s\[\]]+)\[([^\]]*)\]").replace_all(&s, |c: &Captures| {
            let text = c[2].split(',').next().unwrap_or("").trim_end_matches('^').trim();
            format!("[{}]({})", if text.is_empty() { &c[1] } else { text }, &c[1])
        });
        let s = regex!(r"kbd:\[([^\]]+)\]").replace_all(&s, |c: &Captures| format!("`{}`", &c[1]));
        let s = regex!(r"btn:\[([^\]]+)\]").replace_all(&s, |c: &Captures| format!("**[{}]**", &c[1]));
        let s = regex!(r"menu:([^\[]+)\[([^\]]*)\]").replace_all(&s, |c: &Captures| {
            let items: Vec<&str> = std::iter::once(c[1].trim()).chain(c[2].split('>').map(str::trim)).filter(|x| !x.is_empty()).collect();
            format!("**{}**", items.join(" › "))
        });
        let s = regex!(r"footnote(?::[\w-]*)?\[([^\]]*)\]").replace_all(&s, |c: &Captures| {
            if c[1].is_empty() { String::new() } else { format!(" ({})", &c[1]) }
        });
        let s = regex!(r"pass:[a-z,]*\[([^\]]*)\]").replace_all(&s, "$1");
        let s = regex!(r"\[\[[\w:.-]+(?:,[^\]]*)?\]\]|anchor:[\w:.-]+\[[^\]]*\]").replace_all(&s, "");

        // Emphasis: bold (*) and italic (_), unconstrained then constrained; \u{1} marks `**`.
        let s = regex!(r"\*\*(.+?)\*\*").replace_all(&s, "\u{1}$1\u{1}");
        let s = regex!(r"(^|[^\w*\\])\*([^*\s](?:[^*]*?[^*\s])?)\*($|[^\w*])").replace_all(&s, "$1\u{1}$2\u{1}$3");
        let s = regex!(r"__(.+?)__").replace_all(&s, "\u{2}$1\u{2}");
        let s = regex!(r"(^|[^\w_\\])_([^_\s](?:[^_]*?[^_\s])?)_($|[^\w_])").replace_all(&s, "$1\u{2}$2\u{2}$3");
        let s = regex!(r"(^|[^\w#\\(])#([^#\s](?:[^#]*?[^#\s])?)#($|[^\w#])").replace_all(&s, "$1$2$3");
        let s = s.replace('\u{1}', "**").replace('\u{2}', "*");

        regex!("\u{0}(\\d+)\u{0}")
            .replace_all(&s, |c: &Captures| codes[c[1].parse::<usize>().unwrap()].clone())
            .into_owned()
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Delim {
    Listing,
    Literal,
    Example,
    Sidebar,
    Quote,
    Pass,
    Open,
    Table(char),
}

fn delimiter(t: &str) -> Option<Delim> {
    let run = |c: char| t.len() >= 4 && t.chars().all(|x| x == c);
    Some(match t {
        _ if run('-') => Delim::Listing,
        _ if run('.') => Delim::Literal,
        _ if run('=') => Delim::Example,
        _ if run('*') => Delim::Sidebar,
        _ if run('_') => Delim::Quote,
        _ if run('+') => Delim::Pass,
        "--" => Delim::Open,
        "|===" => Delim::Table('|'),
        ",===" => Delim::Table(','),
        ":===" => Delim::Table(':'),
        "!===" => Delim::Table('!'),
        _ => return None,
    })
}

/// Cells on one table line as (content, colspan). Empty if the line continues a cell.
fn split_cells(line: &str, sep: char) -> Vec<(String, usize)> {
    if sep == ',' || sep == ':' {
        return line.split(sep).map(|c| (c.trim().trim_matches('"').to_string(), 1)).collect();
    }
    let spec_re = regex!(r"^(?:(\d+)\+|\d+\*|\d*\.\d+\+)?[<^>]?(?:\.[<^>])?[adehlmsv]?$");
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let mut cells: Vec<(String, usize)> = Vec::new();
    let mut last = 0; // byte index where the current cell's content starts
    let mut open = false; // inside a cell started on this line
    let mut span = 1;
    for (k, &(idx, c)) in chars.iter().enumerate() {
        if c != sep || (k > 0 && chars[k - 1].1 == '\\') {
            continue;
        }
        // The spec is the run of non-space characters immediately before the separator.
        let before = &line[last..idx];
        let spec_start = before.rfind(char::is_whitespace).map_or(0, |p| p + 1);
        let spec = &before[spec_start..];
        if !spec.is_empty() && !spec_re.is_match(spec) {
            continue; // a literal separator character inside cell text
        }
        let content = &before[..spec_start];
        if open {
            cells.push((content.trim().to_string(), span));
        } else if !content.trim().is_empty() {
            return Vec::new(); // text before the first separator: continuation line
        }
        span = regex!(r"^(\d+)\+").captures(spec).and_then(|c| c[1].parse().ok()).unwrap_or(1);
        open = true;
        last = idx + c.len_utf8();
    }
    if open {
        cells.push((line[last..].trim().replace("\\|", "|"), span));
    }
    cells
}

/// Number of columns in a `cols` spec such as `1,2a,3` or `3*` or `2*,1`.
fn count_cols(spec: &str) -> usize {
    let spec = spec.trim().trim_matches('"');
    if let Ok(n) = spec.parse::<usize>() {
        return n;
    }
    spec.split([',', ';'])
        .map(|c| c.trim().split_once('*').and_then(|(n, _)| n.parse().ok()).unwrap_or(1))
        .sum()
}

/// Apply `tag=`, `tags=` and `lines=` include options.
fn select_lines(text: &str, attrs: &BlockAttrs) -> String {
    let lines: Vec<&str> = scan::lines(text).collect();
    if let Some(spec) = attrs.named.get("lines") {
        let mut keep = Vec::new();
        for range in spec.split([',', ';']) {
            let (a, b) = range.split_once("..").unwrap_or((range, range));
            let a: usize = a.trim().parse().unwrap_or(1);
            let b: usize = match b.trim() { "" | "-1" => lines.len(), b => b.parse().unwrap_or(a) };
            keep.extend(lines.iter().skip(a.saturating_sub(1)).take(b.saturating_sub(a) + 1).copied());
        }
        return keep.join("\n");
    }
    let tags: Vec<String> = attrs
        .named
        .get("tag")
        .or(attrs.named.get("tags"))
        .map(|t| t.split([',', ';']).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let tag_re = regex!(r"\b(tag|end)::([\w-]+)\[\]");
    if tags.is_empty() {
        return lines.iter().filter(|l| !tag_re.is_match(l)).copied().collect::<Vec<_>>().join("\n");
    }
    let wanted = |t: &str| tags.iter().any(|x| x == t || x == "**" || x == "*");
    let mut open: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for l in lines {
        if let Some(c) = tag_re.captures(l) {
            if &c[1] == "tag" { open.push(c[2].to_string()) } else { open.retain(|t| t != &c[2]) }
            continue;
        }
        if open.iter().any(|t| wanted(t)) {
            out.push(l);
        }
    }
    out.join("\n")
}

fn shift_sections(text: &str, offset: i32) -> String {
    scan::lines(text)
        .map(|l| {
            let level = l.chars().take_while(|&c| c == '=').count();
            if level > 0 && l[level..].starts_with(' ') {
                let n = (level as i32 + offset).clamp(1, 6) as usize;
                format!("{}{}", "=".repeat(n), &l[level..])
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn heading_gap(body: &[String]) -> Option<String> {
    (!body.is_empty()).then(String::new)
}

fn capitalize(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut c = lower.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Collapse runs of blank lines and trim the ends.
fn tidy(lines: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for l in lines {
        if l.trim().is_empty() && out.last().is_none_or(|p| p.trim().is_empty() || p == ">") {
            continue;
        }
        out.push(if l.trim().is_empty() { String::new() } else { l });
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

pub fn render(cat: &Catalog, file: &Path, text: &str, texts: Texts, image_base: Option<&Path>) -> String {
    let mut s = convert(cat, file, text, texts, image_base).join("\n");
    s.push('\n');
    s
}
