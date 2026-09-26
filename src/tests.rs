use std::fs;
use std::path::Path;

use crate::catalog::*;
use crate::scan;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    write(r, "docs/antora.yml", "name: guide\nversion: '2.0'\ntitle: Guide\nasciidoc:\n  attributes:\n    product: Widget@\n    partials-root: partial$\n");
    write(r, "docs/modules/ROOT/pages/index.adoc", "= Welcome\n\nxref:install/linux.adoc[]\n");
    write(r, "docs/modules/ROOT/pages/install/linux.adoc", "= Linux\n\n== Package Managers\n\n[#apt]\n=== APT\n");
    write(r, "docs/modules/ROOT/partials/note.adoc", "NOTE: hi\n");
    write(r, "docs/modules/ROOT/images/logo.png", "png");
    write(r, "docs/modules/admin/pages/users.adoc", "= Users\n");
    write(r, "old/antora.yml", "name: guide\nversion: '1.0'\n");
    write(r, "old/modules/ROOT/pages/index.adoc", "= Old\n");
    write(r, "api/antora.yml", "name: api\nversion: ~\n");
    write(r, "api/modules/ROOT/pages/overview.adoc", "= API\n");
    dir
}

#[test]
fn parses_resource_ids() {
    let id = parse_resource_id("2.0@guide:admin:partial$a/b.adoc#frag");
    assert_eq!(id.version.as_deref(), Some("2.0"));
    assert_eq!(id.component.as_deref(), Some("guide"));
    assert_eq!(id.module.as_deref(), Some("admin"));
    assert_eq!(id.family.as_deref(), Some("partial"));
    assert_eq!(id.relative, "a/b.adoc");
    assert_eq!(id.fragment.as_deref(), Some("frag"));

    let id = parse_resource_id("api::overview.adoc");
    assert_eq!(id.component.as_deref(), Some("api"));
    assert_eq!(id.module.as_deref(), Some("ROOT"));

    let id = parse_resource_id("admin:users.adoc");
    assert_eq!((id.component, id.module.as_deref()), (None, Some("admin")));

    let id = parse_resource_id("~@api::overview.adoc");
    assert_eq!(id.version.as_deref(), Some(""));
}

#[test]
fn resolves_targets() {
    let dir = fixture();
    let cat = Catalog::scan(&[dir.path().to_path_buf()]);
    assert_eq!(cat.components.len(), 3, "{:?}", cat.errors);

    let page = dir.path().join("docs/modules/ROOT/pages/install/linux.adoc");
    let ctx = cat.locate(&page).expect("page is inside a component");
    assert_eq!((ctx.module.as_str(), ctx.family, ctx.relative.as_str()), ("ROOT", Some(Family::Page), "install/linux.adoc"));

    let ok = |t: &str, k| matches!(cat.resolve(Some(&ctx), &page, t, k), Resolved::Found(_));
    assert!(ok("index.adoc", MacroKind::Xref));
    assert!(ok("index", MacroKind::Xref)); // .adoc optional
    assert!(ok("../index.adoc", MacroKind::Xref)); // relative to current page
    assert!(ok("admin:users.adoc#top", MacroKind::Xref));
    assert!(ok("api::overview.adoc", MacroKind::Xref));
    assert!(ok("partial$note.adoc", MacroKind::Include));
    assert!(ok("{partials-root}note.adoc", MacroKind::Include)); // antora.yml attribute
    assert!(ok("logo.png", MacroKind::Image));
    assert!(ok("1.0@index.adoc", MacroKind::Xref));

    // latest version wins when a component is named without a version
    match cat.resolve(Some(&ctx), &page, "guide::install/linux.adoc", MacroKind::Xref) {
        Resolved::Found(p) => assert!(p.ends_with("docs/modules/ROOT/pages/install/linux.adoc")),
        other => panic!("{other:?}"),
    }

    assert!(matches!(cat.resolve(Some(&ctx), &page, "missing.adoc", MacroKind::Xref), Resolved::NotFound(_)));
    assert!(matches!(cat.resolve(Some(&ctx), &page, "nope::x.adoc", MacroKind::Xref), Resolved::NotFound(_)));
    assert!(matches!(cat.resolve(Some(&ctx), &page, "https://x.io", MacroKind::Xref), Resolved::Skipped));
    assert!(matches!(cat.resolve(Some(&ctx), &page, "{unknown}.adoc", MacroKind::Xref), Resolved::Skipped));
}

#[test]
fn completion_candidates() {
    let dir = fixture();
    let cat = Catalog::scan(&[dir.path().to_path_buf()]);
    let page = dir.path().join("docs/modules/ROOT/pages/index.adoc");
    let ctx = cat.locate(&page).unwrap();
    let labels: Vec<String> = cat.candidates(&ctx, MacroKind::Xref).into_iter().map(|c| c.0).collect();
    for want in ["install/linux.adoc", "admin:users.adoc", "api::overview.adoc"] {
        assert!(labels.iter().any(|l| l == want), "missing {want} in {labels:?}");
    }
    let inc: Vec<String> = cat.candidates(&ctx, MacroKind::Include).into_iter().map(|c| c.0).collect();
    assert!(inc.iter().any(|l| l == "partial$note.adoc"));
}

#[test]
fn scans_macros_and_blocks() {
    let text = "xref:a.adoc[] and image:b.png[]\n----\nxref:skip.adoc[]\ninclude::partial$c.adoc[]\n----\n////\nxref:gone.adoc[]\n////\n// xref:commented.adoc[]\n";
    let found: Vec<(MacroKind, String)> = scan::scan(text).into_iter().map(|t| (t.kind, t.text)).collect();
    assert_eq!(
        found,
        vec![
            (MacroKind::Xref, "a.adoc".into()),
            (MacroKind::Image, "b.png".into()),
            (MacroKind::Include, "partial$c.adoc".into()),
        ]
    );
    assert_eq!(scan::target_before_cursor("see xref:admin:us"), Some((MacroKind::Xref, 9)));
    assert_eq!(scan::attribute_before_cursor("Use {prod"), Some(5));
}

#[test]
fn finds_anchors() {
    let text = "= Linux\n\n== Package Managers\n\n[#apt]\n=== APT\n";
    assert_eq!(scan::find_anchor(text, "apt"), Some(4));
    assert_eq!(scan::find_anchor(text, "_package_managers"), Some(2));
}

#[test]
fn version_order() {
    use std::cmp::Ordering::*;
    assert_eq!(compare_versions("2.0", "1.10"), Greater);
    assert_eq!(compare_versions("1.10", "1.9"), Greater);
    assert_eq!(compare_versions("main", "9.0"), Greater);
    assert_eq!(compare_versions("", "main"), Greater);
}
