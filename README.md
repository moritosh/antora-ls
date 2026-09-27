# antora-ls

A language server (tower-lsp) that understands [Antora](https://antora.org)'s content model. Built for the [zed-antora](../zed-antora) extension, but editor-agnostic — it speaks plain LSP over stdio.

## Features

- Indexes every `antora.yml` in the workspace: components, versions, modules, and the page / partial / example / image / attachment families.
- **Completion** inside `xref:`, `include::` and `image:` targets — local pages, `module:page.adoc`, `component::page.adoc`, `partial$…`, `attachment$…` — plus `{attribute}` completion from `asciidoc.attributes` in `antora.yml`.
- **Go to definition** on any resource ID, jumping to `#fragment` anchors (`[[id]]`, `[#id]`, auto section ids).
- **Hover** shows the target page's title, component version and path.
- **Diagnostics** for broken references (unknown component, version, module, or file), nav files with content Antora ignores, nav files not registered in `antora.yml`, and `nav:` entries pointing at missing files.
- **Find references** to a page, partial, example, image or attachment — from a reference or from inside the file itself.
- **Rename** a resource from any reference to it: the file is moved and every reference is updated, keeping its style (`./` relative, plain include path, omitted `.adoc`). Moving files or folders in the editor's file tree updates references too (`workspace/willRenameFiles`), as long as they stay within their family directory.
- **`page-aliases`**: references to an alias resolve to the page instead of being reported as broken.
- **Playbooks**: local `content.sources` (with `start_path`/`start_paths`) of any `*playbook*.yml` in the workspace are indexed, and its `asciidoc.attributes` apply site-wide (soft-set `@` values yield to components). Remote git sources are skipped.
- Understands `version@component:module:family$path`, `~@` for unversioned, `./`/`../` relative xrefs, optional `.adoc`, latest-version resolution, and `version: true` (git branch).

## Install

```sh
cargo install --path .
```

## Test

```sh
cargo test
```

## Release

Push a tag like `v0.1.0`. The `release` workflow builds `antora-ls-<arch>-<target>.tar.gz|zip` assets, which the Zed extension downloads.

## Limitations

Remote (git) content sources in playbooks aren't fetched, and moving a resource to another module or component doesn't rewrite references. Live preview isn't possible — Zed has no webview API for extensions.

## License and attribution

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).

The Antora resolution logic is derived from the Antora support in the [AsciiDoc plugin for IntelliJ](https://github.com/asciidoctor/asciidoctor-intellij-plugin) (Apache-2.0, © Julien Viet, Erik Pragt, Alexander Schwartz and contributors).
