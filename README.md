# antora-ls

A language server (tower-lsp) that understands [Antora](https://antora.org)'s content model. Built for the [zed-antora](../zed-antora) extension, but editor-agnostic — it speaks plain LSP over stdio.

## Features

- Indexes every `antora.yml` in the workspace: components, versions, modules, and the page / partial / example / image / attachment families.
- **Completion** inside `xref:`, `include::` and `image:` targets — local pages, `module:page.adoc`, `component::page.adoc`, `partial$…`, `attachment$…` — plus `{attribute}` completion from `asciidoc.attributes` in `antora.yml`.
- **Go to definition** on any resource ID, jumping to `#fragment` anchors (`[[id]]`, `[#id]`, auto section ids).
- **Hover** shows the target page's title, component version and path.
- **Diagnostics** for broken references (unknown component, version, module, or file).
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

## Not ported (yet) from the IntelliJ plugin

Rename/refactor of resource IDs, find-usages, playbook-aware multi-repo sources, page-alias awareness, nav-file structure checks.

## License and attribution

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).

The Antora resolution logic is derived from the Antora support in the [AsciiDoc plugin for IntelliJ](https://github.com/asciidoctor/asciidoctor-intellij-plugin) (Apache-2.0, © Julien Viet, Erik Pragt, Alexander Schwartz and contributors).
