//! Structural checks for navigation files: Antora only reads unordered lists
//! (optionally preceded by a `.Title`) and ignores everything else.

use crate::scan::lines;

pub fn check(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut in_comment = false;
    let mut prev_level = 0;

    for (n, line) in lines(text).enumerate() {
        let trimmed = line.trim_end();
        if trimmed.len() >= 4 && trimmed.chars().all(|c| c == '/') {
            in_comment = !in_comment;
            continue;
        }
        if in_comment || trimmed.is_empty() || trimmed.starts_with("//") {
            continue;
        }
        if trimmed.starts_with(':') && trimmed[1..].contains(':') {
            continue; // attribute entry
        }

        let stars = trimmed.chars().take_while(|&c| c == '*').count();
        if stars > 0 {
            let rest = &trimmed[stars..];
            if !rest.starts_with([' ', '\t']) {
                out.push((n, "Nav entries need a space after the `*` marker".into()));
            } else if rest.trim().is_empty() {
                out.push((n, "Empty nav entry".into()));
            } else if stars > prev_level + 1 {
                out.push((n, format!("Nav entry skips a level (`{}` after `{}`)", "*".repeat(stars), "*".repeat(prev_level))));
            }
            prev_level = stars;
            continue;
        }

        let dots = trimmed.chars().take_while(|&c| c == '.').count();
        if dots > 0 && trimmed[dots..].starts_with(' ') {
            out.push((n, "Antora navigation must use unordered lists (`*`), not ordered ones".into()));
            continue;
        }
        if dots == 1 {
            prev_level = 0; // `.Title` starts a new list
            continue;
        }
        out.push((n, "Antora only reads list items and `.Title` lines in nav files; this line is ignored".into()));
    }
    out
}
