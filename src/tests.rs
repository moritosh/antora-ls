use std::fs;
use std::path::Path;

use crate::catalog::*;
use crate::{markdown, nav, preview, refs, scan};

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
    write(
        r,
        "docs/modules/ROOT/pages/install/linux.adoc",
        "= Linux\n:page-aliases: linux.adoc, retired::setup.adoc\n\nxref:../index.adoc[Home]\ninclude::../../partials/note.adoc[]\n\n== Package Managers\n\n[#apt]\n=== APT\n",
    );
    write(r, "docs/modules/ROOT/pages/install/windows.adoc", "= Windows\n\nSee xref:./linux.adoc#apt[] or xref:linux.adoc[old].\n");
    write(r, "docs/modules/ROOT/partials/note.adoc", "NOTE: hi\n");
    write(r, "docs/modules/ROOT/images/logo.png", "png");
    write(r, "docs/modules/admin/pages/users.adoc", "= Users\n\nxref:ROOT:install/linux[] and xref:guide::{missing}x.adoc[]\n");
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
fn block_and_inline_images() {
    let text = "image::logo.png[Logo]\n\nSee image:admin:icon.svg[] inline.\n";
    let found: Vec<(MacroKind, String)> = scan::scan(text).into_iter().map(|t| (t.kind, t.text)).collect();
    assert_eq!(
        found,
        vec![(MacroKind::Image, "logo.png".into()), (MacroKind::Image, "admin:icon.svg".into())]
    );
    assert_eq!(scan::target_before_cursor("image::lo"), Some((MacroKind::Image, 7)));
    assert_eq!(scan::target_before_cursor("see image:lo"), Some((MacroKind::Image, 10)));
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

fn read(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok()
}

#[test]
fn resolves_page_aliases() {
    let dir = fixture();
    let cat = Catalog::scan(&[dir.path().to_path_buf()]);
    let page = dir.path().join("docs/modules/ROOT/pages/index.adoc");
    let ctx = cat.locate(&page).unwrap();
    let linux = canonical(&dir.path().join("docs/modules/ROOT/pages/install/linux.adoc"));
    assert_eq!(cat.resolve(Some(&ctx), &page, "linux.adoc", MacroKind::Xref), Resolved::Alias(linux.clone()));
    // an alias may name a component that no longer exists
    assert_eq!(cat.resolve(Some(&ctx), &page, "retired::setup.adoc", MacroKind::Xref), Resolved::Alias(linux));
    assert!(matches!(cat.resolve(Some(&ctx), &page, "retired::other.adoc", MacroKind::Xref), Resolved::NotFound(_)));
}

#[test]
fn page_alias_header_parsing() {
    let text = "= T\n:page-aliases: a.adoc, \\\n  b:c.adoc,d\n\n:page-aliases: body.adoc\n";
    assert_eq!(scan::page_aliases(text), vec!["a.adoc", "b:c.adoc", "d"]);
    assert!(scan::page_aliases("= T\n\n:page-aliases: late.adoc\n").is_empty());
}

#[test]
fn finds_usages() {
    let dir = fixture();
    let cat = Catalog::scan(&[dir.path().to_path_buf()]);
    let linux = dir.path().join("docs/modules/ROOT/pages/install/linux.adoc");
    let mut found: Vec<(String, usize)> = refs::usages(&cat, &linux, &read)
        .into_iter()
        .map(|u| (u.file.file_name().unwrap().to_string_lossy().into_owned(), u.line))
        .collect();
    found.sort();
    // index (direct), users (other module, no .adoc), windows (./ relative and via alias)
    assert_eq!(found, vec![("index.adoc".into(), 2), ("users.adoc".into(), 2), ("windows.adoc".into(), 2), ("windows.adoc".into(), 2)]);
}

#[test]
fn plans_moves_preserving_reference_style() {
    let dir = fixture();
    let cat = Catalog::scan(&[dir.path().to_path_buf()]);
    let pages = canonical(&dir.path().join("docs/modules/ROOT/pages"));
    let moves = vec![(pages.join("install/linux.adoc"), pages.join("linux-guide.adoc"))];
    let plan = refs::plan_moves(&cat, &moves, &read).unwrap();
    let mut edits: Vec<(String, usize, String)> = plan
        .edits
        .iter()
        .map(|e| (e.file.file_name().unwrap().to_string_lossy().into_owned(), e.line, e.new_text.clone()))
        .collect();
    edits.sort();
    assert_eq!(
        edits,
        vec![
            ("index.adoc".into(), 2, "linux-guide.adoc".into()),
            ("linux.adoc".into(), 3, "./index.adoc".into()),        // own ./.. xref, now one level up
            ("linux.adoc".into(), 4, "../partials/note.adoc".into()), // own plain include
            ("users.adoc".into(), 2, "linux-guide".into()),         // keeps omitted .adoc
            ("windows.adoc".into(), 2, "../linux-guide.adoc".into()), // ./ relative reference
        ]
    );
    assert!(plan.skipped.is_empty());

    // moving across modules is refused
    let other = canonical(&dir.path().join("docs/modules/admin/pages"));
    assert!(refs::plan_moves(&cat, &[(pages.join("index.adoc"), other.join("index.adoc"))], &read).is_err());
}

#[test]
fn nav_structure() {
    let text = ".Guide\n* xref:index.adoc[]\n*** xref:deep.adoc[]\n** ok\n\nSome paragraph\n. ordered\n*\n// comment\n";
    let lines: Vec<usize> = nav::check(text).into_iter().map(|(n, _)| n).collect();
    assert_eq!(lines, vec![2, 5, 6, 7]);
}

#[test]
fn follows_playbook_sources() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    write(
        r,
        "site/antora-playbook.yml",
        "site:\n  title: S\ncontent:\n  sources:\n  - url: ..\n    start_path: content/docs\n  - url: https://example.com/x.git\nasciidoc:\n  attributes:\n    product: Gizmo\n    company: ACME@\n",
    );
    write(r, "content/docs/antora.yml", "name: ext\nversion: ~\nasciidoc:\n  attributes:\n    product: Widget\n    company: Initech\n");
    write(r, "content/docs/modules/ROOT/pages/index.adoc", "= Ext\n");
    write(r, "site/ansible-playbook.yml", "- hosts: all\n");

    let cat = Catalog::scan(&[r.join("site")]);
    assert_eq!(cat.components.len(), 1, "{:?}", cat.errors);
    assert_eq!(cat.remote_sources, vec!["https://example.com/x.git".to_string()]);
    let attrs = &cat.components[0].attributes;
    assert_eq!(attrs["product"], "Gizmo"); // hard-set playbook attribute wins
    assert_eq!(attrs["company"], "Initech"); // soft-set one yields
}

#[test]
fn missing_nav_files_are_reported() {
    let dir = fixture();
    write(dir.path(), "docs/antora.yml", "name: guide\nversion: '2.0'\nnav:\n- modules/ROOT/nav.adoc\n- modules/admin/nav.adoc\n");
    write(dir.path(), "docs/modules/ROOT/nav.adoc", "* xref:index.adoc[]\n");
    let cat = Catalog::scan(&[dir.path().to_path_buf()]);
    let cv = cat.components.iter().find(|c| c.version == "2.0").unwrap();
    assert_eq!(cv.nav.len(), 1);
    assert_eq!(cv.descriptor_problems, vec![(4, "Nav file `modules/admin/nav.adoc` not found".to_string())]);
    assert!(cat.nav_owner(&cv.nav[0]).is_some());
}

#[test]
fn path_helpers() {
    assert_eq!(refs::relative_path(Path::new("/a/b"), Path::new("/a/c/d.adoc")), "../c/d.adoc");
    assert_eq!(refs::relative_path(Path::new("/a"), Path::new("/a/d.adoc")), "d.adoc");
    assert_eq!(refs::normalize_new_name("./x/y", Family::Page).unwrap(), "x/y.adoc");
    assert_eq!(refs::normalize_new_name("logo.svg", Family::Image).unwrap(), "logo.svg");
    assert!(refs::normalize_new_name("../escape.adoc", Family::Page).is_err());
}

#[test]
fn converts_asciidoc_to_markdown() {
    let dir = fixture();
    write(dir.path(), "docs/modules/ROOT/partials/steps.adoc", "intro\n// tag::steps[]\n= Steps\n\nDo *this*.\n// end::steps[]\noutro\n");
    let page = dir.path().join("docs/modules/ROOT/pages/demo.adoc");
    let text = r#"= Demo Page
:description: ignored
:flavor: vanilla

Hello {product}, *bold* _it_ `+{raw}+` and **x**y in {flavor}.
See xref:install/linux.adoc[] and xref:admin:users.adoc[the users] and xref:nope.adoc[gone].

ifdef::flavor[]
Flavor is set.
endif::[]
ifndef::flavor[]
Hidden.
endif::[]

include::partial$steps.adoc[tag=steps,leveloffset=+1]

.Setup
* one
** nested
* [x] done

term:: definition

[NOTE]
====
Careful.
====

TIP: Short tip.

[source,sh]
----
echo {flavor}
----

[cols="1,2",options="header"]
|===
|A |B
|1 |two
2+|span
|===

image::logo.png[Logo]
"#;
    let cat = Catalog::scan(&[dir.path().to_path_buf()]);
    let md = markdown::render(&cat, &page, text, &read, None);
    let relative = markdown::render(&cat, &page, "image::logo.png[]", &read, Some(&canonical_lenient(&dir.path().join("docs/out"))));
    assert_eq!(relative, "![logo](<../modules/ROOT/images/logo.png>)\n");
    let anchored = markdown::render(&cat, &page, "xref:install/linux.adoc#apt[APT]", &read, None);
    assert!(anchored.ends_with("linux.adoc#L9>)\n"), "{anchored}");
    let linux = canonical(&dir.path().join("docs/modules/ROOT/pages/install/linux.adoc"));
    let users = canonical(&dir.path().join("docs/modules/admin/pages/users.adoc"));
    let logo = canonical(&dir.path().join("docs/modules/ROOT/images/logo.png"));
    let expected = format!(
        r#"# Demo Page

Hello Widget, **bold** *it* `{{raw}}` and **x**y in vanilla.
See [Linux](<{linux}>) and [the users](<{users}>) and gone ⚠️.

Flavor is set.

## Steps

Do **this**.

**Setup**

- one
  - nested
- [x] done

- **term** — definition

> **Note:**
>
> Careful.

> **Tip:** Short tip.

```sh
echo {{flavor}}
```

| A | B |
| --- | --- |
| 1 | two |
| span |  |

![Logo](<{logo}>)
"#,
        linux = linux.display(),
        users = users.display(),
        logo = logo.display()
    );
    assert_eq!(md, expected, "\n--- got ---\n{md}");
}

#[test]
fn finds_the_preview_runtime_on_unix_and_windows() {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    write(r, "bin/node", "");
    write(r, "win/node.exe", "");
    write(r, "home/.bun/bin/bun.exe", "");
    let path = |d: &str| Some(std::env::join_paths([r.join(d)]).unwrap());

    assert_eq!(preview::runtime_in(path("bin"), None, ""), Some(r.join("bin/node")));
    // Windows names them node.exe and bun.exe; Bun's install directory comes before Node.
    assert_eq!(preview::runtime_in(path("win"), None, ".exe"), Some(r.join("win/node.exe")));
    assert_eq!(preview::runtime_in(path("win"), Some(r.join("home")), ".exe"), Some(r.join("home/.bun/bin/bun.exe")));
    assert_eq!(preview::runtime_in(path("win"), None, ""), None);
}
