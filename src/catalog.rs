//! Antora content model: discovers component versions (antora.yml), indexes their
//! resources by module and family, and resolves Antora resource IDs.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Family {
    Page,
    Partial,
    Example,
    Image,
    Attachment,
}

impl Family {
    pub const ALL: [Family; 5] = [
        Family::Page,
        Family::Partial,
        Family::Example,
        Family::Image,
        Family::Attachment,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Family::Page => "page",
            Family::Partial => "partial",
            Family::Example => "example",
            Family::Image => "image",
            Family::Attachment => "attachment",
        }
    }

    pub fn dir(self) -> &'static str {
        match self {
            Family::Page => "pages",
            Family::Partial => "partials",
            Family::Example => "examples",
            Family::Image => "images",
            Family::Attachment => "attachments",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.id() == s)
    }

    fn from_dir(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.dir() == s)
    }
}

/// The macro a target appears in; decides the default family and resolution rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacroKind {
    Xref,
    Include,
    Image,
}

impl MacroKind {
    pub fn default_family(self) -> Option<Family> {
        match self {
            MacroKind::Xref => Some(Family::Page),
            MacroKind::Image => Some(Family::Image),
            MacroKind::Include => None,
        }
    }

    /// Families that are sensible to offer in completion for this macro.
    pub fn completion_families(self) -> &'static [Family] {
        match self {
            MacroKind::Xref => &[Family::Page, Family::Attachment],
            MacroKind::Include => &[Family::Partial, Family::Example, Family::Page],
            MacroKind::Image => &[Family::Image],
        }
    }
}

#[derive(Debug, Default)]
pub struct Module {
    /// family -> (relative path with forward slashes -> absolute path)
    pub resources: BTreeMap<Family, BTreeMap<String, PathBuf>>,
    /// Files directly in the module directory (typically nav.adoc).
    pub files: Vec<PathBuf>,
}

#[derive(Debug)]
pub struct ComponentVersion {
    pub name: String,
    /// Empty string means unversioned (`version: ~`).
    pub version: String,
    pub title: Option<String>,
    /// Directory containing antora.yml (the content source root).
    pub root: PathBuf,
    pub descriptor: PathBuf,
    pub attributes: BTreeMap<String, String>,
    pub modules: BTreeMap<String, Module>,
    /// Nav files registered under `nav:` in antora.yml.
    pub nav: Vec<PathBuf>,
    /// Problems in antora.yml as (line, message).
    pub descriptor_problems: Vec<(usize, String)>,
}

impl ComponentVersion {
    /// Directory holding a module's resources of one family.
    pub fn family_dir(&self, module: &str, family: Family) -> PathBuf {
        self.root.join("modules").join(module).join(family.dir())
    }

    pub fn display(&self) -> String {
        let v = if self.version.is_empty() { "~" } else { &self.version };
        match &self.title {
            Some(t) => format!("{t} ({}@{v})", self.name),
            None => format!("{}@{v}", self.name),
        }
    }
}

/// Where a file sits inside the Antora structure.
#[derive(Debug, Clone)]
pub struct FileContext {
    pub cv: usize,
    pub module: String,
    /// None for files in the module root (e.g. nav.adoc).
    pub family: Option<Family>,
    /// Path relative to the family directory.
    pub relative: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ResourceId {
    pub version: Option<String>,
    pub component: Option<String>,
    pub module: Option<String>,
    pub family: Option<String>,
    pub relative: String,
    pub fragment: Option<String>,
}

/// Parse `[version@][[component:]module:][family$]relative[#fragment]`.
pub fn parse_resource_id(input: &str) -> ResourceId {
    let (id, fragment) = match input.split_once('#') {
        Some((a, b)) => (a, Some(b.to_string())),
        None => (input, None),
    };

    // Version: an '@' that appears before any ':' or '$'.
    let first_sep = id.find([':', '$']).unwrap_or(id.len());
    let (version, rest) = match id[..first_sep].find('@') {
        Some(at) => (Some(id[..at].to_string()), &id[at + 1..]),
        None => (None, id),
    };

    // Family: text before '$', after the last ':' preceding it.
    let (coords, family, relative) = match rest.find('$') {
        Some(d) => {
            let head = &rest[..d];
            let rel = &rest[d + 1..];
            match head.rfind(':') {
                Some(c) => (Some(&head[..c]), Some(head[c + 1..].to_string()), rel),
                None => (None, Some(head.to_string()), rel),
            }
        }
        None => match rest.rfind(':') {
            Some(c) => (Some(&rest[..c]), None, &rest[c + 1..]),
            None => (None, None, rest),
        },
    };

    let (component, module) = match coords {
        None => (None, None),
        Some(c) => match c.split_once(':') {
            Some((comp, m)) => (Some(comp.to_string()), Some(module_or_root(m))),
            None => (None, Some(module_or_root(c))),
        },
    };

    ResourceId {
        // `~@` explicitly addresses the unversioned version.
        version: version.map(|v| if v == "~" { String::new() } else { v }),
        component,
        module,
        family,
        relative: relative.to_string(),
        fragment,
    }
}

fn module_or_root(m: &str) -> String {
    if m.is_empty() { "ROOT".into() } else { m.into() }
}

#[derive(Debug, PartialEq)]
pub enum Resolved {
    Found(PathBuf),
    /// Reached through a `page-aliases` entry of the target page.
    Alias(PathBuf),
    NotFound(String),
    /// Can't be judged (external URL, unresolved attribute, file outside Antora).
    Skipped,
}

impl Resolved {
    pub fn file(&self) -> Option<&Path> {
        match self {
            Resolved::Found(p) | Resolved::Alias(p) => Some(p),
            _ => None,
        }
    }
}

/// A `page-aliases` entry, normalized to full coordinates.
#[derive(Debug)]
pub struct Alias {
    /// As written in the page header.
    pub raw: String,
    pub component: String,
    pub version: String,
    pub module: String,
    pub relative: String,
    pub target: PathBuf,
}

#[derive(Deserialize)]
struct Descriptor {
    name: Option<String>,
    #[serde(default)]
    version: serde_yaml::Value,
    title: Option<String>,
    #[serde(default)]
    asciidoc: Option<AsciidocSection>,
    #[serde(default)]
    nav: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Playbook {
    content: PlaybookContent,
    #[serde(default)]
    asciidoc: Option<AsciidocSection>,
}

#[derive(Deserialize)]
struct PlaybookContent {
    #[serde(default)]
    sources: Vec<PlaybookSource>,
}

#[derive(Deserialize)]
struct PlaybookSource {
    url: String,
    start_path: Option<String>,
    #[serde(default)]
    start_paths: serde_yaml::Value,
}

#[derive(Deserialize, Default)]
struct AsciidocSection {
    #[serde(default)]
    attributes: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Default)]
pub struct Catalog {
    pub components: Vec<ComponentVersion>,
    pub aliases: Vec<Alias>,
    pub errors: Vec<(PathBuf, String)>,
    /// Playbooks whose content sources were followed.
    pub playbooks: Vec<PathBuf>,
    /// Remote (git) content sources that can't be indexed locally.
    pub remote_sources: Vec<String>,
}

const SKIP_DIRS: &[&str] = &["node_modules", "target", "build", "dist", ".cache"];

impl Catalog {
    pub fn scan(roots: &[PathBuf]) -> Self {
        let mut cat = Catalog::default();
        let mut descriptors = BTreeSet::new();
        let mut playbooks = Vec::new();
        for root in roots {
            walk(root, &mut descriptors, &mut playbooks);
        }

        // Follow local content sources of playbooks found in the workspace.
        let mut covered: Vec<PathBuf> = roots.iter().map(|r| canonical(r)).collect();
        let mut site_attributes: BTreeMap<String, (Option<String>, bool)> = BTreeMap::new();
        for pb in playbooks {
            let Some(playbook) = load_playbook(&pb) else { continue };
            let base = pb.parent().unwrap_or(Path::new("."));
            for src in &playbook.content.sources {
                let Some(dir) = local_source_dir(&src.url, base) else {
                    cat.remote_sources.push(src.url.clone());
                    continue;
                };
                for start in start_paths(src) {
                    let dir = canonical(&dir.join(start));
                    if !covered.iter().any(|c| dir.starts_with(c)) {
                        walk(&dir, &mut descriptors, &mut Vec::new());
                        covered.push(dir);
                    }
                }
            }
            for (k, v) in playbook.asciidoc.unwrap_or_default().attributes {
                if let Some(attr) = site_attribute(v) {
                    site_attributes.entry(k).or_insert(attr);
                }
            }
            cat.playbooks.push(pb);
        }

        for descriptor in descriptors {
            match load_component(&descriptor) {
                Ok(cv) => cat.components.push(cv),
                Err(e) => cat.errors.push((descriptor, e)),
            }
        }

        // Playbook attributes apply site-wide; soft-set ones (trailing `@`) yield to components.
        for cv in &mut cat.components {
            for (name, (value, soft)) in &site_attributes {
                match value {
                    Some(v) if !*soft || !cv.attributes.contains_key(name) => {
                        cv.attributes.insert(name.clone(), v.clone());
                    }
                    None if !*soft => {
                        cv.attributes.remove(name);
                    }
                    _ => {}
                }
            }
        }

        cat.aliases = cat.components.iter().flat_map(page_aliases).collect();
        cat
    }

    /// Every AsciiDoc source that can contain references: pages, partials, examples and nav files.
    pub fn adoc_files(&self) -> Vec<PathBuf> {
        let mut out = BTreeSet::new();
        for cv in &self.components {
            for module in cv.modules.values() {
                for fam in [Family::Page, Family::Partial, Family::Example] {
                    out.extend(module.resources.get(&fam).into_iter().flat_map(|r| r.values()).cloned());
                }
                out.extend(module.files.iter().cloned());
            }
            out.extend(cv.nav.iter().cloned());
        }
        out.into_iter().filter(|p| is_adoc(p)).collect()
    }

    /// Indexed resource files inside `dir`.
    pub fn resources_under(&self, dir: &Path) -> Vec<PathBuf> {
        self.components
            .iter()
            .flat_map(|cv| cv.modules.values())
            .flat_map(|m| m.resources.values())
            .flat_map(|r| r.values())
            .filter(|p| p.starts_with(dir))
            .cloned()
            .collect()
    }

    /// The component version whose antora.yml registers `file` as a nav file.
    pub fn nav_owner(&self, file: &Path) -> Option<&ComponentVersion> {
        self.components.iter().find(|cv| cv.nav.iter().any(|n| n == file))
    }

    fn alias(&self, component: &str, version: Option<&str>, module: &str, relative: &str) -> Option<&PathBuf> {
        self.aliases
            .iter()
            .find(|a| {
                a.component == component
                    && a.module == module
                    && a.relative == relative
                    && version.is_none_or(|v| a.version == v)
            })
            .map(|a| &a.target)
    }

    /// Index of the latest version of a component.
    pub fn latest(&self, name: &str) -> Option<usize> {
        self.components
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name == name)
            .max_by(|a, b| compare_versions(&a.1.version, &b.1.version))
            .map(|(i, _)| i)
    }

    pub fn find_version(&self, name: &str, version: &str) -> Option<usize> {
        self.components.iter().position(|c| c.name == name && c.version == version)
    }

    pub fn locate(&self, file: &Path) -> Option<FileContext> {
        let file = canonical(file);
        let mut best: Option<(usize, FileContext)> = None;
        for (i, cv) in self.components.iter().enumerate() {
            let Ok(rest) = file.strip_prefix(cv.root.join("modules")) else { continue };
            let parts: Vec<String> =
                rest.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            if parts.len() < 2 {
                continue;
            }
            let (family, relative) = match Family::from_dir(&parts[1]) {
                Some(f) if parts.len() > 2 => (Some(f), parts[2..].join("/")),
                _ => (None, parts[1..].join("/")),
            };
            let depth = cv.root.components().count();
            if best.as_ref().is_none_or(|(d, _)| depth > *d) {
                best = Some((depth, FileContext { cv: i, module: parts[0].clone(), family, relative }));
            }
        }
        best.map(|(_, c)| c)
    }

    pub fn attributes_for(&self, ctx: Option<&FileContext>) -> Option<&BTreeMap<String, String>> {
        ctx.map(|c| &self.components[c.cv].attributes)
    }

    /// Replace `{name}` with component attributes. Returns None if any remain unresolved.
    pub fn substitute(&self, ctx: Option<&FileContext>, target: &str) -> Option<String> {
        if !target.contains('{') {
            return Some(target.to_string());
        }
        let attrs = self.attributes_for(ctx)?;
        let mut out = String::new();
        let mut rest = target;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let close = rest[open..].find('}')? + open;
            out.push_str(attrs.get(&rest[open + 1..close])?);
            rest = &rest[close + 1..];
        }
        out.push_str(rest);
        Some(out)
    }

    pub fn resolve(
        &self,
        ctx: Option<&FileContext>,
        current_file: &Path,
        raw_target: &str,
        kind: MacroKind,
    ) -> Resolved {
        if raw_target.contains("://") || raw_target.starts_with("data:") || raw_target.is_empty() {
            return Resolved::Skipped;
        }
        let Some(target) = self.substitute(ctx, raw_target) else { return Resolved::Skipped };

        // include:: without a family is a plain path relative to the including file.
        if kind == MacroKind::Include && !target.contains('$') {
            let base = current_file.parent().unwrap_or(Path::new("."));
            let p = base.join(&target);
            return if p.is_file() {
                Resolved::Found(canonical(&p))
            } else {
                Resolved::NotFound(format!("Included file not found: {target}"))
            };
        }

        let id = parse_resource_id(&target);

        let family = match &id.family {
            Some(f) => match Family::from_id(f) {
                Some(f) => f,
                None => return Resolved::NotFound(format!("Unknown resource family `{f}$`")),
            },
            None => match kind.default_family() {
                Some(f) => f,
                None => return Resolved::Skipped,
            },
        };

        // Pick the component version.
        let cv_index = match (&id.component, &id.version, ctx) {
            (Some(comp), Some(v), _) => self.find_version(comp, v),
            (Some(comp), None, _) => self.latest(comp),
            (None, Some(v), Some(c)) => self.find_version(&self.components[c.cv].name, v),
            (None, None, Some(c)) => Some(c.cv),
            (None, _, None) => return Resolved::Skipped,
        };
        let component = id.component.clone().or(ctx.map(|c| self.components[c.cv].name.clone()));

        let module = match (&id.module, &id.component, ctx) {
            (Some(m), _, _) => m.clone(),
            (None, None, Some(c)) => c.module.clone(),
            _ => "ROOT".into(),
        };

        // Antora 3: ./ and ../ in xrefs are relative to the current page.
        let mut relative = id.relative.clone();
        if (relative.starts_with("./") || relative.starts_with("../")) && id.module.is_none() {
            if let Some(c) = ctx.filter(|c| c.family == Some(family)) {
                let dir = c.relative.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                relative = normalize(&format!("{dir}/{relative}"));
            }
        }
        if family == Family::Page && !relative.contains('.') {
            relative.push_str(".adoc");
        }

        let cv = cv_index.map(|i| &self.components[i]);
        if let Some(p) = cv
            .and_then(|cv| cv.modules.get(&module))
            .and_then(|m| m.resources.get(&family))
            .and_then(|r| r.get(&relative))
        {
            return Resolved::Found(p.clone());
        }
        if family == Family::Page {
            let version = cv.map(|cv| cv.version.as_str()).or(id.version.as_deref());
            if let Some(p) = component.as_deref().and_then(|c| self.alias(c, version, &module, &relative)) {
                return Resolved::Alias(p.clone());
            }
        }

        let Some(cv) = cv else {
            return Resolved::NotFound(match (component, &id.version) {
                (Some(c), Some(v)) => format!("Unknown component version `{v}@{c}`"),
                (Some(c), None) => format!("Unknown component `{c}`"),
                _ => "Unknown component".into(),
            });
        };
        if !cv.modules.contains_key(&module) {
            return Resolved::NotFound(format!("Module `{module}` not found in {}", cv.display()));
        }
        Resolved::NotFound(format!(
            "{} `{relative}` not found in {}:{module}",
            capitalize(family.id()),
            cv.display()
        ))
    }

    /// Completion candidates as (insert text, detail).
    pub fn candidates(&self, ctx: &FileContext, kind: MacroKind) -> Vec<(String, String)> {
        let current = &self.components[ctx.cv];
        let default = kind.default_family();
        let mut out = Vec::new();
        let mut targets: Vec<(usize, String)> = vec![(ctx.cv, String::new())];
        for (i, cv) in self.components.iter().enumerate() {
            if cv.name != current.name && self.latest(&cv.name) == Some(i) {
                targets.push((i, format!("{}:", cv.name)));
            }
        }
        for (cv_index, comp_prefix) in targets {
            let cv = &self.components[cv_index];
            for (mod_name, module) in &cv.modules {
                let mod_prefix = if comp_prefix.is_empty() {
                    if *mod_name == ctx.module { String::new() } else { format!("{mod_name}:") }
                } else if mod_name == "ROOT" {
                    format!("{comp_prefix}:")
                } else {
                    format!("{comp_prefix}{mod_name}:")
                };
                for &fam in kind.completion_families() {
                    let fam_prefix =
                        if Some(fam) == default { String::new() } else { format!("{}$", fam.id()) };
                    for rel in module.resources.get(&fam).into_iter().flat_map(|r| r.keys()) {
                        out.push((
                            format!("{mod_prefix}{fam_prefix}{rel}"),
                            format!("{} · {mod_name} · {}", cv.display(), fam.id()),
                        ));
                    }
                }
            }
        }
        out
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

pub fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Like `canonical`, but also works for paths that don't exist yet by
/// canonicalizing the nearest existing ancestor.
pub fn canonical_lenient(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => canonical_lenient(parent).join(name),
        _ => p.to_path_buf(),
    }
}

pub fn is_adoc(p: &Path) -> bool {
    p.extension().is_some_and(|e| e == "adoc" || e == "asciidoc")
}

fn walk(root: &Path, descriptors: &mut BTreeSet<PathBuf>, playbooks: &mut Vec<PathBuf>) {
    let walker = WalkDir::new(root).into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        e.depth() == 0 || !(name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()))
    });
    for entry in walker.flatten().filter(|e| e.file_type().is_file()) {
        let name = entry.file_name().to_string_lossy();
        if name == "antora.yml" {
            descriptors.insert(canonical(entry.path()));
        } else if name.contains("playbook") && (name.ends_with(".yml") || name.ends_with(".yaml")) {
            playbooks.push(canonical(entry.path()));
        }
    }
}

/// Parses an Antora playbook; anything else named `*playbook*.yml` (e.g. Ansible) is ignored.
fn load_playbook(path: &Path) -> Option<Playbook> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str(&text).ok()
}

/// A content source URL that points at the local filesystem, resolved against the playbook dir.
fn local_source_dir(url: &str, base: &Path) -> Option<PathBuf> {
    let remote = url.contains("://")
        || url.starts_with("git@")
        || (url.contains(':') && !url.starts_with(['/', '.', '~']) && !cfg!(windows));
    if remote {
        return None;
    }
    let home = || std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    Some(match url.strip_prefix('~') {
        Some(rest) => home().join(rest.trim_start_matches('/')),
        None => base.join(url),
    })
}

/// `start_path` / `start_paths` of a source; globs are reduced to their fixed prefix.
fn start_paths(src: &PlaybookSource) -> Vec<String> {
    let mut paths: Vec<String> = match &src.start_paths {
        serde_yaml::Value::String(s) => s.split(',').map(|p| p.trim().to_string()).collect(),
        serde_yaml::Value::Sequence(seq) => seq.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    };
    paths.extend(src.start_path.clone());
    if paths.is_empty() {
        paths.push(String::new());
    }
    paths
        .into_iter()
        .map(|p| {
            p.split('/')
                .take_while(|seg| !seg.contains(['*', '?', '{', '[']))
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect()
}

/// A playbook attribute as (value, soft-set); `None` value unsets the attribute.
fn site_attribute(v: serde_yaml::Value) -> Option<(Option<String>, bool)> {
    Some(match v {
        serde_yaml::Value::String(s) => match s.strip_suffix('@') {
            Some(soft) => (Some(soft.to_string()), true),
            None => (Some(s), false),
        },
        serde_yaml::Value::Number(n) => (Some(n.to_string()), false),
        serde_yaml::Value::Bool(true) => (Some(String::new()), false),
        serde_yaml::Value::Bool(false) | serde_yaml::Value::Null => (None, false),
        _ => return None,
    })
}

/// Reads the `page-aliases` of every page in a component version.
fn page_aliases(cv: &ComponentVersion) -> Vec<Alias> {
    let mut out = Vec::new();
    for (module, m) in &cv.modules {
        for page in m.resources.get(&Family::Page).into_iter().flat_map(|r| r.values()) {
            if !is_adoc(page) {
                continue;
            }
            for raw in crate::scan::page_aliases(&read_header(page)) {
                if raw.contains('{') {
                    continue;
                }
                let id = parse_resource_id(&raw);
                let mut relative = id.relative;
                if !relative.contains('.') {
                    relative.push_str(".adoc");
                }
                let module = match (id.module, &id.component) {
                    (Some(m), _) => m,
                    (None, Some(_)) => "ROOT".into(),
                    (None, None) => module.clone(),
                };
                out.push(Alias {
                    raw: raw.clone(),
                    component: id.component.unwrap_or_else(|| cv.name.clone()),
                    version: id.version.unwrap_or_else(|| cv.version.clone()),
                    module,
                    relative,
                    target: page.clone(),
                });
            }
        }
    }
    out
}

/// The document header: everything up to the first blank line after some content.
fn read_header(path: &Path) -> String {
    let Ok(file) = std::fs::File::open(path) else { return String::new() };
    let mut out = String::new();
    for line in std::io::BufReader::new(file).lines().map_while(|l| l.ok()) {
        if line.trim().is_empty() && !out.is_empty() {
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Antora-ish ordering: unversioned and named versions rank above numeric ones;
/// numeric versions compare segment by segment.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    fn rank(v: &str) -> u8 {
        if v.is_empty() {
            2
        } else if v.trim_start_matches('v').starts_with(|c: char| c.is_ascii_digit()) {
            0
        } else {
            1
        }
    }
    match rank(a).cmp(&rank(b)) {
        Ordering::Equal if rank(a) == 0 => {
            let seg = |v: &str| -> Vec<u64> {
                v.trim_start_matches('v')
                    .split(['.', '-'])
                    .map(|s| s.parse().unwrap_or(0))
                    .collect()
            };
            seg(a).cmp(&seg(b))
        }
        Ordering::Equal => b.cmp(a),
        o => o,
    }
}

fn load_component(descriptor: &Path) -> Result<ComponentVersion, String> {
    let text = std::fs::read_to_string(descriptor).map_err(|e| e.to_string())?;
    let d: Descriptor = serde_yaml::from_str(&text).map_err(|e| e.to_string())?;
    let name = d.name.ok_or("antora.yml is missing `name`")?;
    let root = canonical(descriptor.parent().unwrap_or(Path::new(".")));

    let version = match &d.version {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(true) => git_branch(&root).unwrap_or_else(|| "HEAD".into()),
        _ => String::new(),
    };

    let mut attributes = BTreeMap::new();
    for (k, v) in d.asciidoc.unwrap_or_default().attributes {
        let value = match v {
            serde_yaml::Value::String(s) => s.trim_end_matches('@').to_string(),
            serde_yaml::Value::Number(n) => n.to_string(),
            serde_yaml::Value::Bool(true) => String::new(),
            _ => continue, // false / null unset the attribute
        };
        attributes.insert(k, value);
    }

    let mut modules = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(root.join("modules")) {
        for entry in entries.flatten().filter(|e| e.path().is_dir()) {
            let mut module = Module::default();
            if let Ok(files) = std::fs::read_dir(entry.path()) {
                module.files = files.flatten().map(|f| f.path()).filter(|p| p.is_file()).collect();
                module.files.sort();
            }
            for fam in Family::ALL {
                let dir = entry.path().join(fam.dir());
                let mut files = BTreeMap::new();
                for f in WalkDir::new(&dir).into_iter().flatten().filter(|f| f.file_type().is_file()) {
                    if let Ok(rel) = f.path().strip_prefix(&dir) {
                        let rel = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
                        // Hidden files never count; underscore-prefixed pages aren't published.
                        let hidden = rel.split('/').any(|s| {
                            s.starts_with('.') || (fam == Family::Page && s.starts_with('_'))
                        });
                        if !hidden {
                            files.insert(rel, f.path().to_path_buf());
                        }
                    }
                }
                if !files.is_empty() {
                    module.resources.insert(fam, files);
                }
            }
            modules.insert(entry.file_name().to_string_lossy().into_owned(), module);
        }
    }

    let mut nav = Vec::new();
    let mut descriptor_problems = Vec::new();
    for entry in d.nav.unwrap_or_default() {
        let path = canonical(&root.join(&entry));
        if path.is_file() {
            nav.push(path);
        } else {
            let line = text.lines().position(|l| l.contains(entry.as_str())).unwrap_or(0);
            descriptor_problems.push((line, format!("Nav file `{entry}` not found")));
        }
    }

    Ok(ComponentVersion {
        name,
        version,
        title: d.title,
        descriptor: canonical(descriptor),
        root,
        attributes,
        modules,
        nav,
        descriptor_problems,
    })
}

fn git_branch(start: &Path) -> Option<String> {
    let head = start.ancestors().map(|a| a.join(".git/HEAD")).find(|p| p.is_file())?;
    let text = std::fs::read_to_string(head).ok()?;
    text.trim().strip_prefix("ref: refs/heads/").map(str::to_string)
}
