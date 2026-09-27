//! Cross-file operations: finding references to a resource and planning the
//! reference updates when resources are renamed or moved.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::catalog::{canonical, canonical_lenient, parse_resource_id, Catalog, Family, MacroKind, Resolved};
use crate::scan::{self, Target};

/// Supplies a document's current text: the editor buffer if open, else the file on disk.
pub type Texts<'a> = &'a dyn Fn(&Path) -> Option<String>;

#[derive(Debug)]
pub struct Usage {
    pub file: PathBuf,
    pub line: usize,
    /// UTF-16 columns of the whole target.
    pub start: u32,
    pub end: u32,
}

/// A replacement within one line, in UTF-16 columns.
#[derive(Debug)]
pub struct Edit {
    pub file: PathBuf,
    pub line: usize,
    pub start: u32,
    pub end: u32,
    pub new_text: String,
}

#[derive(Debug, Default)]
pub struct Plan {
    pub edits: Vec<Edit>,
    /// References that need updating but use attributes, as (file, line).
    pub skipped: Vec<(PathBuf, usize)>,
}

/// Every reference in the workspace that resolves to `resource`.
pub fn usages(cat: &Catalog, resource: &Path, texts: Texts) -> Vec<Usage> {
    let resource = canonical(resource);
    let mut out = Vec::new();
    for file in cat.adoc_files() {
        let Some(text) = texts(&file) else { continue };
        let lines: Vec<&str> = scan::lines(&text).collect();
        let ctx = cat.locate(&file);
        for t in scan::scan(&text) {
            let resolved = cat.resolve(ctx.as_ref(), &file, &t.text, t.kind);
            if resolved.file() == Some(resource.as_path()) {
                let line = lines[t.line];
                out.push(Usage {
                    file: file.clone(),
                    line: t.line,
                    start: scan::utf16_col(line, t.start),
                    end: scan::utf16_col(line, t.end),
                });
            }
        }
    }
    out
}

/// Byte range (within the line) of the part of a target that names the file:
/// `relative` of the resource ID, or the whole path of a plain include.
pub fn relative_span(t: &Target) -> (usize, usize) {
    let id_part = t.text.split_once('#').map_or(t.text.as_str(), |(id, _)| id);
    let rel_len = if is_plain_include(t) { id_part.len() } else { parse_resource_id(id_part).relative.len() };
    let end = t.start + id_part.len();
    (end - rel_len, end)
}

fn is_plain_include(t: &Target) -> bool {
    t.kind == MacroKind::Include && !t.text.contains('$')
}

/// Where a resource may be moved: within its family directory of the same module,
/// so references only need their relative path changed.
pub fn family_relative(cat: &Catalog, file: &Path) -> Option<(PathBuf, Family, String)> {
    let ctx = cat.locate(file)?;
    let family = ctx.family?;
    let dir = cat.components[ctx.cv].family_dir(&ctx.module, family);
    Some((dir, family, ctx.relative))
}

/// Plan reference updates for moving files (`from -> to`, absolute paths). Every
/// move must stay inside the resource's family directory.
pub fn plan_moves(cat: &Catalog, moves: &[(PathBuf, PathBuf)], texts: Texts) -> Result<Plan, String> {
    let mut moved: HashMap<PathBuf, (PathBuf, String)> = HashMap::new();
    for (from, to) in moves {
        let from = canonical(from);
        let (dir, family, _) = family_relative(cat, &from)
            .ok_or_else(|| format!("{} is not an Antora resource", from.display()))?;
        let to = canonical_lenient(to);
        let new_rel = to
            .strip_prefix(&dir)
            .ok()
            .and_then(slash_path)
            .ok_or_else(|| format!("{} can only move within its `{}` directory", from.display(), family.dir()))?;
        moved.insert(from, (to, new_rel));
    }

    let mut plan = Plan::default();
    for file in cat.adoc_files() {
        let file_moved = moved.get(&file).map(|(to, _)| to);
        let Some(text) = texts(&file) else { continue };
        let lines: Vec<&str> = scan::lines(&text).collect();
        let ctx = cat.locate(&file);
        let new_file = file_moved.unwrap_or(&file);

        for t in scan::scan(&text) {
            let Resolved::Found(target) = cat.resolve(ctx.as_ref(), &file, &t.text, t.kind) else { continue };
            let target_moved = moved.get(&target);
            if target_moved.is_none() && file_moved.is_none() {
                continue;
            }
            let new_target = target_moved.map_or(&target, |(to, _)| to);
            let (start, end) = relative_span(&t);
            let line = lines[t.line];
            let old = &line[start..end];

            let new = if is_plain_include(&t) {
                relative_path(new_file.parent().unwrap_or(Path::new("/")), new_target)
            } else if old.starts_with("./") || old.starts_with("../") {
                let rel = relative_path(new_file.parent().unwrap_or(Path::new("/")), new_target);
                let rel = if rel.starts_with("../") { rel } else { format!("./{rel}") };
                keep_extension_style(old, rel)
            } else if let Some((_, new_rel)) = target_moved {
                keep_extension_style(old, new_rel.clone())
            } else {
                continue; // family-relative reference from a moved file: unaffected
            };

            if new == old {
                continue;
            }
            if old.contains('{') {
                plan.skipped.push((file.clone(), t.line));
                continue;
            }
            plan.edits.push(Edit {
                file: file.clone(),
                line: t.line,
                start: scan::utf16_col(line, start),
                end: scan::utf16_col(line, end),
                new_text: new,
            });
        }
    }
    Ok(plan)
}

/// Page references may omit `.adoc`; keep doing so if the original did.
fn keep_extension_style(old: &str, new: String) -> String {
    let old_name = old.rsplit('/').next().unwrap_or(old);
    if !old_name.contains('.') {
        if let Some(stem) = new.strip_suffix(".adoc") {
            return stem.to_string();
        }
    }
    new
}

fn slash_path(p: &Path) -> Option<String> {
    let parts: Vec<String> = p
        .components()
        .map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Path from directory `from` to `to`, with forward slashes.
pub fn relative_path(from: &Path, to: &Path) -> String {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); from.len() - common];
    parts.extend(to[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned()));
    parts.join("/")
}

/// Validate a new family-relative name typed by the user.
pub fn normalize_new_name(name: &str, family: Family) -> Result<String, String> {
    let name = name.trim().trim_start_matches("./");
    if name.is_empty() || name.starts_with('/') || name.contains('\\') {
        return Err("Enter a path relative to the family directory, e.g. `install/linux.adoc`".into());
    }
    if name.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return Err("The new path must stay inside its family directory".into());
    }
    let mut name = name.to_string();
    if family == Family::Page && !name.rsplit('/').next().unwrap_or("").contains('.') {
        name.push_str(".adoc");
    }
    Ok(name)
}
