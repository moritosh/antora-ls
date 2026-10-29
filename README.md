# antora-ls

A language server for [Antora](https://antora.org) documentation sites. It understands Antora's content model — components, versions, modules, resource families and resource IDs — and speaks plain LSP over stdio. It's built for the [antora-zed](https://github.com/moritosh/antora-zed) extension but works with any LSP client.

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

Requirements: an Antora playbook in the workspace with Antora 3 installed in the project (`node_modules`, including pnpm layouts), and [Bun](https://bun.sh) or Node. The runtime is found as `bun` on the PATH, then `~/.bun/bin/bun`, then `node` (on Windows: `bun.exe`, `%USERPROFILE%\.bun\bin\bun.exe`, `node.exe`). The preview server is `preview/antora-preview.js`, embedded in the binary; it uses only built-in modules and the project's Antora. It stops when the language server exits.

### Markdown preview

*Antora: Open Markdown preview* renders the page as Markdown for an editor's built-in Markdown preview. It opens `Antora Preview.md`, a read-only file in the system temp directory, on which you open the editor's Markdown preview once. There is one preview per workspace, and it follows you: when you open, edit or click into another page, it switches to that page, and it updates as you type.

The conversion uses the site index: xrefs become links to the target's source file (at the anchor's line), `include::` directives are inlined (honoring `tag`/`tags`, `lines` and `leveloffset`), images resolve to the image files, and attributes — including Antora's `page-*` attributes — and `ifdef`/`ifndef` conditionals are applied. Headings, lists, checklists, description lists, code blocks, tables, block titles, quotes and inline formatting map to their Markdown equivalents. Markdown can't express everything: admonitions, sidebars and examples become quotes, cell spans are flattened, and `ifeval` blocks are always shown. Use the live site preview for the real rendering.

## Configuration

Initialization options (in Zed: `lsp.antora-ls.initialization_options`):

```json
{ "preview": { "runtime": "/path/to/bun-or-node" } }
```

## Install

**In Zed you don't need these steps:** the [antora-zed](https://github.com/moritosh/antora-zed) extension downloads antora-ls by itself. For other editors, install the binary from the [latest release](https://github.com/moritosh/antora-ls/releases/latest) as below, then set up `antora-ls` as the language server for AsciiDoc files. It takes no arguments and talks LSP over stdin and stdout.

### macOS

1. Download the binary into `~/.local/bin`. On Apple Silicon:
   ```sh
   mkdir -p ~/.local/bin
   curl -fsSL https://github.com/moritosh/antora-ls/releases/latest/download/antora-ls-aarch64-apple-darwin.tar.gz | tar xz -C ~/.local/bin
   ```
   On an Intel Mac, use `antora-ls-x86_64-apple-darwin.tar.gz` instead. If you download the archive in a browser rather than with `curl`, macOS blocks the unsigned binary; run `xattr -d com.apple.quarantine ~/.local/bin/antora-ls` once to allow it.
2. If `~/.local/bin` isn't on your `PATH` yet, add it and open a new terminal:
   ```sh
   echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zprofile
   ```
3. Check that it runs: `antora-ls --version`.
4. For the live preview, install [Bun](https://bun.sh) (`curl -fsSL https://bun.sh/install | bash`) or Node (`brew install node`, or the installer from [nodejs.org](https://nodejs.org)).

### Windows

1. In PowerShell, download the binary and add its folder to your user `PATH`:
   ```powershell
   $dir = "$env:LOCALAPPDATA\Programs\antora-ls"
   Invoke-WebRequest https://github.com/moritosh/antora-ls/releases/latest/download/antora-ls-x86_64-pc-windows-msvc.zip -OutFile "$env:TEMP\antora-ls.zip"
   Expand-Archive "$env:TEMP\antora-ls.zip" -DestinationPath $dir -Force
   [Environment]::SetEnvironmentVariable("Path", [Environment]::GetEnvironmentVariable("Path", "User") + ";$dir", "User")
   ```
   There's no ARM build; on Windows on ARM, this x64 build runs under emulation.
2. Open a new terminal and check that it runs: `antora-ls --version`. Restart your editor too, so it sees the new `PATH`.
3. For the live preview, install [Bun](https://bun.sh) (`powershell -c "irm bun.sh/install.ps1 | iex"`) or Node (`winget install OpenJS.NodeJS.LTS`, or the installer from [nodejs.org](https://nodejs.org)).

### Linux

1. Download the binary into `~/.local/bin`; this picks the x86_64 or ARM64 build:
   ```sh
   mkdir -p ~/.local/bin
   curl -fsSL https://github.com/moritosh/antora-ls/releases/latest/download/antora-ls-$(uname -m)-unknown-linux-gnu.tar.gz | tar xz -C ~/.local/bin
   ```
   The binaries need glibc 2.39 or later (Ubuntu 24.04, Debian 13, Fedora 40 or newer). On older distributions, [build from source](#from-source).
2. If `~/.local/bin` isn't on your `PATH`, add `export PATH="$HOME/.local/bin:$PATH"` to `~/.profile` and log in again.
3. Check that it runs: `antora-ls --version`.
4. For the live preview, install [Bun](https://bun.sh) (`curl -fsSL https://bun.sh/install | bash`; it needs `unzip`) or Node (your distribution's `nodejs` package, or [nvm](https://github.com/nvm-sh/nvm)).

### From source

With Rust installed from [rustup.rs](https://rustup.rs), on any of the three:

```sh
cargo install --git https://github.com/moritosh/antora-ls
```

In a clone, `cargo install --path .` does the same.

### The docs project

Completion, navigation and diagnostics work on any folder with `antora.yml` files. The live preview also needs Antora 3 installed in the project that contains the playbook:

```sh
npm i -D antora    # or: pnpm add -D antora, yarn add -D antora, bun add -d antora
```

## Test

```sh
cargo test
```

## Release

Bump `version` in `Cargo.toml` (and `Cargo.lock`), commit, and push a tag like `v0.2.4`. The `release` workflow builds `antora-ls-<arch>-<target>.tar.gz|zip` assets for macOS, Linux and Windows, which the Zed extension downloads.

Releases are versioned independently of the projects below; see [Used by](#used-by) for how a release reaches them.

## Limitations

- Remote (git) content sources in playbooks aren't indexed for completion and diagnostics; only local sources are.
- Moving a resource to another module, component or family doesn't rewrite references (renames and moves within the family directory do).
- Undoing a rename in the editor reverts the text edits but not the file move; rename again to reverse it.
- The live preview runs in a browser; editors like Zed have no API for extensions to embed one.
- Because pages are rendered on demand, a search extension such as Lunr only indexes page titles in the preview, and Antora extensions that post-process converted pages don't run on them.

## Used by

- [antora-zed](https://github.com/moritosh/antora-zed), the Zed extension, runs the language server. It downloads the latest antora-ls release for the platform (unless one is configured or on the `PATH`), so users get a new release without an extension update.
- [antora-intellij](https://github.com/moritosh/antora-intellij), the IntelliJ plugin, doesn't use the language server: it bundles `preview/antora-preview.js` from the antora-ls release named by `antoraLsVersion` in its `gradle.properties`. Changes to the preview server reach it when that version is raised and the plugin is released.

## License and attribution

Apache License 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE).

The Antora resolution logic is derived from the Antora support in the [AsciiDoc plugin for IntelliJ](https://github.com/asciidoctor/asciidoctor-intellij-plugin) (Apache-2.0, © Julien Viet, Erik Pragt, Alexander Schwartz and contributors).
