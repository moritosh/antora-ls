//! Finds xref/include/image targets in AsciiDoc text, with positions.

use std::sync::OnceLock;

use regex::Regex;

use crate::catalog::MacroKind;

#[derive(Debug, Clone)]
pub struct Target {
    pub kind: MacroKind,
    pub line: usize,
    /// Byte offsets of the target text within the line.
    pub start: usize,
    pub end: usize,
    pub text: String,
}

fn inline_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|[^\\\w])(xref:|image::?)([^\s\[\]]+)\[").unwrap())
}

fn include_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^include::([^\[]+)\[").unwrap())
}

/// Macro prefix immediately before the cursor, e.g. `...xref:comp:mod:pa|`.
fn partial_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(xref:|include::|image::?)([^\s\[\]]*)$").unwrap())
}

pub fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l))
}

#[derive(PartialEq)]
enum Block {
    None,
    Comment,     // ////
    Literal,     // ---- or .... (includes still apply; macros don't)
    Passthrough, // ++++
}

pub fn scan(text: &str) -> Vec<Target> {
    let mut out = Vec::new();
    let mut block = Block::None;
    let mut fence = String::new();

    for (n, line) in lines(text).enumerate() {
        let trimmed = line.trim_end();
        let is_fence = |c: char| trimmed.len() >= 4 && trimmed.chars().all(|x| x == c);
        if block != Block::None {
            if trimmed == fence {
                block = Block::None;
            }
        } else if is_fence('/') {
            block = Block::Comment;
            fence = trimmed.into();
            continue;
        } else if is_fence('-') || is_fence('.') {
            block = Block::Literal;
            fence = trimmed.into();
            continue;
        } else if is_fence('+') {
            block = Block::Passthrough;
            fence = trimmed.into();
            continue;
        }
        if matches!(block, Block::Comment | Block::Passthrough) || line.starts_with("//") {
            continue;
        }

        if let Some(c) = include_re().captures(line) {
            let m = c.get(1).unwrap();
            out.push(Target {
                kind: MacroKind::Include,
                line: n,
                start: m.start(),
                end: m.end(),
                text: m.as_str().to_string(),
            });
            continue;
        }
        if block == Block::Literal {
            continue;
        }
        for c in inline_re().captures_iter(line) {
            let kind = if c[1].starts_with("xref") { MacroKind::Xref } else { MacroKind::Image };
            let m = c.get(2).unwrap();
            out.push(Target {
                kind,
                line: n,
                start: m.start(),
                end: m.end(),
                text: m.as_str().to_string(),
            });
        }
    }
    out
}

/// If the text before the cursor is inside a macro target, return its kind and
/// the byte offset where the target starts.
pub fn target_before_cursor(line_prefix: &str) -> Option<(MacroKind, usize)> {
    let c = partial_re().captures(line_prefix)?;
    let kind = match &c[1] {
        "xref:" => MacroKind::Xref,
        "include::" => {
            if c.get(1).unwrap().start() != 0 {
                return None;
            }
            MacroKind::Include
        }
        _ => MacroKind::Image,
    };
    Some((kind, c.get(2).unwrap().start()))
}

/// If the cursor is right after an unclosed `{`, return the offset after the brace.
pub fn attribute_before_cursor(line_prefix: &str) -> Option<usize> {
    let open = line_prefix.rfind('{')?;
    let after = &line_prefix[open + 1..];
    after
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        .then_some(open + 1)
}

pub fn utf16_col(line: &str, byte: usize) -> u32 {
    line[..byte.min(line.len())].encode_utf16().count() as u32
}

pub fn byte_of_utf16(line: &str, col: u32) -> usize {
    let mut units = 0u32;
    for (i, ch) in line.char_indices() {
        if units >= col {
            return i;
        }
        units += ch.len_utf16() as u32;
    }
    line.len()
}

/// Find the line of an anchor (`[[id]]`, `[#id]`, `anchor:id[]`, or an
/// auto-generated section id like `_section_title`).
pub fn find_anchor(text: &str, id: &str) -> Option<usize> {
    let explicit = [format!("[[{id}]]"), format!("[[{id},"), format!("[#{id}"), format!("anchor:{id}[")];
    for (n, line) in lines(text).enumerate() {
        if explicit.iter().any(|p| line.contains(p.as_str())) {
            return Some(n);
        }
        let t = line.trim_start_matches('=');
        if t.len() < line.len() && t.starts_with(' ') && auto_id(t.trim()) == id {
            return Some(n);
        }
    }
    None
}

fn auto_id(title: &str) -> String {
    let mut s = String::from("_");
    for c in title.to_lowercase().chars() {
        if c.is_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('_') {
            s.push('_');
        }
    }
    s.trim_end_matches('_').to_string()
}

/// Entries of `:page-aliases:` in the document header (comma-separated, `\` continues a line).
pub fn page_aliases(text: &str) -> Vec<String> {
    let mut lines = lines(text).skip_while(|l| l.trim().is_empty());
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            break; // end of header
        }
        let Some(value) = line.strip_prefix(":page-aliases:") else { continue };
        let mut value = value.trim().to_string();
        while let Some(head) = value.strip_suffix('\\') {
            value = head.trim_end().to_string();
            match lines.next() {
                Some(next) => value.push_str(&format!(" {}", next.trim())),
                None => break,
            }
        }
        return value.split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
    }
    Vec::new()
}

pub fn document_title(text: &str) -> Option<String> {
    lines(text).find_map(|l| l.strip_prefix("= ").map(|t| t.trim().to_string()))
}
