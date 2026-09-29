# antora-ls

A language server for [Antora](https://antora.org) documentation sites. It understands Antora's content model — components, versions, modules, resource families and resource IDs — and speaks plain LSP over stdio. It's built for the [zed-antora](https://github.com/moritosh/zed-antora) extension but works with any LSP client.

## Features

### Project model

- Indexes every `antora.yml` in the workspace: components, versions, modules, and the page / partial / example / image / attachment families.
- Follows **playbooks**: the local `content.sources` (including `start_path` / `start_paths`) of any `*playbook*.yml` in the workspace are indexed even when they live outside the workspace folder, and the playbook's `asciidoc.attributes` apply site-wide (soft-set `@` values yield to the component's own). Remote git sources are skipped.
- Reads **`page-aliases`** from page headers, so references to an alias resolve to the page instead of being reported as broken.
- Re-indexes when files are created, deleted or renamed, when `antora.yml` or a playbook changes, and when a saved page's aliases change.

### Editing

- **Completion** inside `xref:`, `include::`, `image:` and `image::` targets. It offers what fits the macro — pages and attachments for `xref:`, partials, examples and pages for `include::`, images for `image:` — across modules (`module:page.adoc`) and the latest version of other components (`component::page.adoc`), with the right `partial$` / `example$` / `attachment$` prefix. Typing `{` completes attributes from `asciidoc.attributes`. Triggered by `:`, `$`, `@`, `/` and `{`.
- **Go to definition** on any reference opens the target, at the `#fragment` anchor if there is one (`[[id]]`, `[#id]`, `anchor:id[]`, or an auto-generated section id). The whole target is reported as the link, so Cmd/Ctrl-hover underlines all of it.
- **Hover** shows the target page's title, component version, module and path, and notes when the reference goes through a page alias.
- **Find references** to a page, partial, example, image or attachment — invoked on a reference, or anywhere in the file itself. Covers pages, partials, examples and nav files.
- **Rename** a resource from any reference to it: enter the new path relative to its family directory (`install/linux` or `setup/linux.adoc`), and the file is moved and every reference updated. Each reference keeps its style — `./` and `../` relative xrefs, plain include paths, omitted `.adoc`, module and component prefixes. References inside the moved file are rewritten too.
- **Moving files or folders** in the editor's file tree updates references the same way (`workspace/willRenameFiles`), as long as they stay inside their family directory.

### Diagnostics

- **Broken references**: unknown component, version, module or file, and unknown resource families.
- **Nav files**: content Antora ignores (anything but list items and `.Title` lines), ordered lists, empty entries, and entries that skip a nesting level.
- Nav files in a module that aren't registered under `nav:` in `antora.yml` (information).
- `nav:` entries in `antora.yml` that point at missing files (reported on `antora.yml`).

### Resource IDs understood

`version@component:module:family$path#fragment` in full or in part, `~@` for the unversioned version, `./` and `../` xrefs relative to the current page, plain `include::` paths relative to the including file, optional `.adoc` on page references, latest-version resolution when no version is given, `version: true` (the current git branch), and `{attribute}` references in targets.

## Previews

Both are offered as code actions on any file in the site.

### Live site preview

*Antora: Open live site preview* builds the site with the project's own Antora and opens the current page in the browser. After that it behaves like a dev server with hot reload:

- **Fast start**: the browser opens right away and shows the build's progress, which also appears in the editor's status bar, then switches to the page. Cancelling the progress in the editor stops the preview; open it again to restart it. Pages are converted when first viewed rather than all up front; every page's header is still read, so xref text and navigation are complete. A site of 11,000 pages is ready in about 4 seconds instead of 80.

- **Built from HEAD**: local content sources are read from the working tree, whatever branches or tags the playbook names, so you see what you're editing. Sources whose start path doesn't exist in HEAD are skipped; remote sources are built as the playbook defines them.
- **Live**: unsaved edits are sent as you type; only the changed page is re-rendered and its article is swapped into the open page without a reload. Changes to partials and examples re-render the pages that include them; nav changes rebuild the navigation. New files, deletions, and changes to `antora.yml` or the playbook's `supplemental_files` trigger a full rebuild, one at a time, while the previous build stays viewable with a *Rebuilding* badge (a replaced UI bundle zip is picked up when the language server, and with it the preview, restarts). Saved changes from other tools (git, other editors) are picked up by a file watcher. If a build fails, the error is shown in the browser and the editor; saving a file retries.
- **Follows the cursor**: the browser scrolls to the block you're editing, and switches to another page when you start editing it.
- **In memory**: the site is served from memory on `127.0.0.1` and nothing is written to the project. Updates are only accepted from the language server (they carry a per-session token).

Requirements: an Antora playbook in the workspace with Antora 3 installed in the project (`node_modules`, including pnpm layouts), and [Bun](https://bun.sh) or Node. The runtime is found as `bun` on the PATH, then `~/.bun/bin/bun`, then `node`. The preview server is `preview/antora-preview.js`, embedded in the binary; it uses only built-in modules and the project's Antora. It stops when the language server exits.

### Markdown preview

*Antora: Open Markdown preview* renders the page as Markdown for an editor's built-in Markdown preview. It opens `Antora Preview.md`, a read-only file in the system temp directory, on which you open the editor's Markdown preview once. There is one preview per workspace, and it follows you: when you open, edit or click into another page, it switches to that page, and it updates as you type.

The conversion uses the site index: xrefs become links to the target's source file (at the anchor's line), `include::` directives are inlined (honoring `tag`/`tags`, `lines` and `leveloffset`), images resolve to the image files, and attributes — including Antora's `page-*` attributes — and `ifdef`/`ifndef` conditionals are applied. Headings, lists, checklists, description lists, code blocks, tables, block titles, quotes and inline formatting map to their Markdown equivalents. Markdown can't express everything: admonitions, sidebars and examples become quotes, cell spans are flattened, and `ifeval` blocks are always shown. Use the live site preview for the real rendering.

## Configuration

Initialization options (in Zed: `lsp.antora-ls.initialization_options`):

```json
{ "preview": { "runtime": "/path/to/bun-or-node" } }
```

## Install

```sh
cargo install --path .
```

## Test

```sh
cargo test
```

## Release

Push a tag like `v0.2.1`. The `release` workflow builds `antora-ls-<arch>-<target>.tar.gz|zip` assets for macOS, Linux and Windows, which the Zed extension downloads. The extension downloads without authentication, so this only works once the repository is public.

## Limitations

- Remote (git) content sources in playbooks aren't indexed for completion and diagnostics; only local sources are.
- Moving a resource to another module, component or family doesn't rewrite references (renames and moves within the family directory do).
- Undoing a rename in the editor reverts the text edits but not the file move; rename again to reverse it.
- The live preview runs in a browser; editors like Zed have no API for extensions to embed one.
- Because pages are rendered on demand, a search extension such as Lunr only indexes page titles in the preview, and Antora extensions that post-process converted pages don't run on them.

## License and attribution

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).

The Antora resolution logic is derived from the Antora support in the [AsciiDoc plugin for IntelliJ](https://github.com/asciidoctor/asciidoctor-intellij-plugin) (Apache-2.0, © Julien Viet, Erik Pragt, Alexander Schwartz and contributors).
