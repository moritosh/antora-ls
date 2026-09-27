mod catalog;
mod nav;
mod refs;
mod scan;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use catalog::{canonical, Catalog, MacroKind, Resolved};

#[derive(Default)]
struct State {
    roots: Vec<PathBuf>,
    catalog: Arc<Catalog>,
    docs: HashMap<Url, String>,
    /// antora.yml files we've published diagnostics for.
    descriptors: HashSet<Url>,
    /// Client can apply file renames inside a WorkspaceEdit.
    can_rename_files: bool,
}

/// Open documents' text keyed by canonical path, for work that reads many files.
type OpenTexts = HashMap<PathBuf, String>;

fn text_of(open: &OpenTexts, path: &Path) -> Option<String> {
    open.get(path).cloned().or_else(|| std::fs::read_to_string(path).ok())
}

fn invalid(msg: impl Into<String>) -> tower_lsp::jsonrpc::Error {
    tower_lsp::jsonrpc::Error::invalid_params(msg.into())
}

struct Backend {
    client: Client,
    state: Arc<RwLock<State>>,
}

const MAX_COMPLETIONS: usize = 2000;

impl Backend {
    async fn rescan(&self) {
        let roots = self.state.read().await.roots.clone();
        let catalog = tokio::task::spawn_blocking(move || Catalog::scan(&roots))
            .await
            .unwrap_or_default();
        for (path, err) in &catalog.errors {
            self.client
                .log_message(MessageType::WARNING, format!("antora-ls: {}: {err}", path.display()))
                .await;
        }
        for pb in &catalog.playbooks {
            self.client
                .log_message(MessageType::INFO, format!("antora-ls: using content sources from {}", pb.display()))
                .await;
        }
        for url in &catalog.remote_sources {
            self.client
                .log_message(MessageType::INFO, format!("antora-ls: skipping remote content source {url}"))
                .await;
        }
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "antora-ls: indexed {} component version(s), {} page alias(es)",
                    catalog.components.len(),
                    catalog.aliases.len()
                ),
            )
            .await;
        self.state.write().await.catalog = Arc::new(catalog);
        self.publish_descriptors().await;
        self.publish_all().await;
    }

    /// Problems in antora.yml files (e.g. missing nav files).
    async fn publish_descriptors(&self) {
        let (current, stale) = {
            let mut state = self.state.write().await;
            let current: Vec<(Url, Vec<Diagnostic>)> = state
                .catalog
                .components
                .iter()
                .filter_map(|cv| {
                    let uri = Url::from_file_path(&cv.descriptor).ok()?;
                    let diags = cv
                        .descriptor_problems
                        .iter()
                        .map(|(line, msg)| warning(Range::new(Position::new(*line as u32, 0), Position::new(*line as u32, u32::MAX)), msg.clone()))
                        .collect();
                    Some((uri, diags))
                })
                .collect();
            let now: HashSet<Url> = current.iter().map(|(u, _)| u.clone()).collect();
            let stale: Vec<Url> = state.descriptors.difference(&now).cloned().collect();
            state.descriptors = now;
            (current, stale)
        };
        for uri in stale {
            self.client.publish_diagnostics(uri, vec![], None).await;
        }
        for (uri, diags) in current {
            self.client.publish_diagnostics(uri, diags, None).await;
        }
    }

    async fn snapshot(&self) -> (Arc<Catalog>, OpenTexts) {
        let state = self.state.read().await;
        let open = state
            .docs
            .iter()
            .filter_map(|(uri, text)| Some((canonical(&uri.to_file_path().ok()?), text.clone())))
            .collect();
        (state.catalog.clone(), open)
    }

    /// The resource a request is about: the reference under the cursor, else the current file.
    async fn subject(&self, uri: &Url, pos: Position) -> Option<PathBuf> {
        if let Some((_, resolved, _)) = self.target_at(uri, pos).await {
            return resolved.file().map(Path::to_path_buf);
        }
        let path = canonical(&uri.to_file_path().ok()?);
        self.state.read().await.catalog.locate(&path).map(|_| path)
    }

    async fn publish_all(&self) {
        let uris: Vec<Url> = self.state.read().await.docs.keys().cloned().collect();
        for uri in uris {
            self.publish(&uri).await;
        }
    }

    async fn publish(&self, uri: &Url) {
        let diagnostics = {
            let state = self.state.read().await;
            let (Some(text), Ok(path)) = (state.docs.get(uri), uri.to_file_path()) else { return };
            diagnostics(&state.catalog, &path, text)
        };
        self.client.publish_diagnostics(uri.clone(), diagnostics, None).await;
    }

    /// Build a WorkspaceEdit from planned edits, plus optional file renames.
    fn workspace_edit(plan: &refs::Plan, renames: &[(PathBuf, PathBuf)]) -> Option<WorkspaceEdit> {
        let mut by_file: BTreeMap<&Path, Vec<OneOf<TextEdit, AnnotatedTextEdit>>> = BTreeMap::new();
        for e in &plan.edits {
            let range = Range::new(Position::new(e.line as u32, e.start), Position::new(e.line as u32, e.end));
            by_file.entry(&e.file).or_default().push(OneOf::Left(TextEdit::new(range, e.new_text.clone())));
        }
        let mut ops = Vec::new();
        for (file, edits) in by_file {
            let uri = Url::from_file_path(file).ok()?;
            ops.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier { uri, version: None },
                edits,
            }));
        }
        for (from, to) in renames {
            ops.push(DocumentChangeOperation::Op(ResourceOp::Rename(RenameFile {
                old_uri: Url::from_file_path(from).ok()?,
                new_uri: Url::from_file_path(to).ok()?,
                options: Some(RenameFileOptions { overwrite: Some(false), ignore_if_exists: None }),
                annotation_id: None,
            })));
        }
        Some(WorkspaceEdit { document_changes: Some(DocumentChanges::Operations(ops)), ..Default::default() })
    }

    async fn warn_skipped(&self, skipped: &[(PathBuf, usize)]) {
        if skipped.is_empty() {
            return;
        }
        let list: Vec<String> = skipped.iter().map(|(f, l)| format!("{}:{}", f.display(), l + 1)).collect();
        self.client
            .show_message(
                MessageType::WARNING,
                format!("antora-ls: {} reference(s) use attributes and were not updated: {}", list.len(), list.join(", ")),
            )
            .await;
    }

    /// The macro target under the cursor, resolved.
    async fn target_at(&self, uri: &Url, pos: Position) -> Option<(scan::Target, Resolved, Range)> {
        let state = self.state.read().await;
        let text = state.docs.get(uri)?;
        let path = uri.to_file_path().ok()?;
        let line = scan::lines(text).nth(pos.line as usize)?;
        let byte = scan::byte_of_utf16(line, pos.character);
        let target = scan::scan(text)
            .into_iter()
            .find(|t| t.line == pos.line as usize && t.start <= byte && byte <= t.end)?;
        let ctx = state.catalog.locate(&path);
        let resolved = state.catalog.resolve(ctx.as_ref(), &path, &target.text, target.kind);
        let range = Range::new(
            Position::new(pos.line, scan::utf16_col(line, target.start)),
            Position::new(pos.line, scan::utf16_col(line, target.end)),
        );
        Some((target, resolved, range))
    }
}

fn warning(range: Range, message: String) -> Diagnostic {
    Diagnostic {
        range,
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("antora".into()),
        message,
        ..Default::default()
    }
}

fn whole_line(n: usize, line: &str) -> Range {
    Range::new(Position::new(n as u32, 0), Position::new(n as u32, scan::utf16_col(line, line.len())))
}

fn diagnostics(catalog: &Catalog, path: &Path, text: &str) -> Vec<Diagnostic> {
    let path = canonical(path);
    let ctx = catalog.locate(&path);
    let lines: Vec<&str> = scan::lines(text).collect();
    let mut out: Vec<Diagnostic> = scan::scan(text)
        .into_iter()
        .filter_map(|t| match catalog.resolve(ctx.as_ref(), &path, &t.text, t.kind) {
            Resolved::NotFound(msg) => {
                let line = lines[t.line];
                let range = Range::new(
                    Position::new(t.line as u32, scan::utf16_col(line, t.start)),
                    Position::new(t.line as u32, scan::utf16_col(line, t.end)),
                );
                Some(warning(range, msg))
            }
            _ => None,
        })
        .collect();

    if catalog.nav_owner(&path).is_some() {
        out.extend(nav::check(text).into_iter().map(|(n, msg)| warning(whole_line(n, lines[n]), msg)));
    } else if ctx.as_ref().is_some_and(|c| c.family.is_none()) && catalog::is_adoc(&path) {
        let first = lines.first().copied().unwrap_or("");
        out.push(Diagnostic {
            severity: Some(DiagnosticSeverity::INFORMATION),
            ..warning(
                whole_line(0, first),
                "This file is not listed under `nav` in antora.yml, so Antora ignores it".into(),
            )
        });
    }
    out
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let mut roots: Vec<PathBuf> = params
            .workspace_folders
            .unwrap_or_default()
            .iter()
            .filter_map(|f| f.uri.to_file_path().ok())
            .collect();
        #[allow(deprecated)]
        if roots.is_empty() {
            roots.extend(params.root_uri.and_then(|u| u.to_file_path().ok()));
        }
        let can_rename_files = params
            .capabilities
            .workspace
            .and_then(|w| w.workspace_edit)
            .and_then(|e| e.resource_operations)
            .is_some_and(|ops| ops.contains(&ResourceOperationKind::Rename));
        {
            let mut state = self.state.write().await;
            state.roots = roots;
            state.can_rename_files = can_rename_files;
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "antora-ls".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
                        ..Default::default()
                    },
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(
                        [":", "$", "@", "/", "{"].iter().map(|s| s.to_string()).collect(),
                    ),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: None,
                    file_operations: Some(WorkspaceFileOperationsServerCapabilities {
                        will_rename: Some(FileOperationRegistrationOptions {
                            filters: vec![FileOperationFilter {
                                scheme: Some("file".into()),
                                pattern: FileOperationPattern {
                                    glob: "**/modules/**".into(),
                                    matches: None,
                                    options: None,
                                },
                            }],
                        }),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let watchers = ["**/antora.yml", "**/modules/**", "**/*playbook*.yml", "**/*playbook*.yaml"]
            .into_iter()
            .map(|g| FileSystemWatcher { glob_pattern: GlobPattern::String(g.into()), kind: None })
            .collect();
        let options = DidChangeWatchedFilesRegistrationOptions { watchers };
        let registration = Registration {
            id: "antora-watch".into(),
            method: "workspace/didChangeWatchedFiles".into(),
            register_options: serde_json::to_value(options).ok(),
        };
        let _ = self.client.register_capability(vec![registration]).await;
        self.rescan().await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, p: DidOpenTextDocumentParams) {
        let uri = p.text_document.uri;
        self.state.write().await.docs.insert(uri.clone(), p.text_document.text);
        self.publish(&uri).await;
    }

    async fn did_change(&self, p: DidChangeTextDocumentParams) {
        let uri = p.text_document.uri;
        if let Some(change) = p.content_changes.into_iter().last() {
            self.state.write().await.docs.insert(uri.clone(), change.text);
        }
        self.publish(&uri).await;
    }

    async fn did_save(&self, p: DidSaveTextDocumentParams) {
        let uri = &p.text_document.uri;
        let aliases_changed = {
            let state = self.state.read().await;
            match (state.docs.get(uri), uri.to_file_path()) {
                (Some(text), Ok(path)) => {
                    let path = canonical(&path);
                    let mut now: Vec<String> = scan::page_aliases(text).into_iter().filter(|a| !a.contains('{')).collect();
                    let mut indexed: Vec<String> =
                        state.catalog.aliases.iter().filter(|a| a.target == path).map(|a| a.raw.clone()).collect();
                    now.sort();
                    indexed.sort();
                    now != indexed
                }
                _ => false,
            }
        };
        if aliases_changed || uri.path().ends_with("antora.yml") {
            self.rescan().await;
        }
    }

    async fn did_close(&self, p: DidCloseTextDocumentParams) {
        self.state.write().await.docs.remove(&p.text_document.uri);
        self.client.publish_diagnostics(p.text_document.uri, vec![], None).await;
    }

    async fn did_change_watched_files(&self, p: DidChangeWatchedFilesParams) {
        // Content edits don't change the index; only creates, deletes and descriptors do.
        let relevant = p.changes.iter().any(|c| {
            let path = c.uri.path();
            c.typ != FileChangeType::CHANGED || path.ends_with("antora.yml") || path.contains("playbook")
        });
        if relevant {
            self.rescan().await;
        }
    }

    async fn completion(&self, p: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = p.text_document_position.text_document.uri;
        let pos = p.text_document_position.position;
        let state = self.state.read().await;
        let (Some(text), Ok(path)) = (state.docs.get(&uri), uri.to_file_path()) else {
            return Ok(None);
        };
        let Some(line) = scan::lines(text).nth(pos.line as usize) else { return Ok(None) };
        let prefix = &line[..scan::byte_of_utf16(line, pos.character)];
        let Some(ctx) = state.catalog.locate(&path) else { return Ok(None) };

        let edit_range = |start: usize| Range::new(Position::new(pos.line, scan::utf16_col(line, start)), pos);

        if let Some((kind, start)) = scan::target_before_cursor(prefix) {
            let typed = &prefix[start..];
            let range = edit_range(start);
            let items: Vec<CompletionItem> = state
                .catalog
                .candidates(&ctx, kind)
                .into_iter()
                .filter(|(label, _)| label.contains(typed.rsplit(['/', ':', '$']).next().unwrap_or("")))
                .take(MAX_COMPLETIONS)
                .map(|(label, detail)| CompletionItem {
                    kind: Some(match kind {
                        MacroKind::Image => CompletionItemKind::FILE,
                        _ => CompletionItemKind::REFERENCE,
                    }),
                    detail: Some(detail),
                    filter_text: Some(label.clone()),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range, label.clone()))),
                    label,
                    ..Default::default()
                })
                .collect();
            let incomplete = items.len() >= MAX_COMPLETIONS;
            return Ok(Some(CompletionResponse::List(CompletionList { is_incomplete: incomplete, items })));
        }

        if let Some(start) = scan::attribute_before_cursor(prefix) {
            let range = edit_range(start);
            let items = state.catalog.components[ctx.cv]
                .attributes
                .iter()
                .map(|(name, value)| CompletionItem {
                    label: name.clone(),
                    kind: Some(CompletionItemKind::CONSTANT),
                    detail: Some(value.clone()),
                    text_edit: Some(CompletionTextEdit::Edit(TextEdit::new(range, format!("{name}}}")))),
                    ..Default::default()
                })
                .collect();
            return Ok(Some(CompletionResponse::Array(items)));
        }
        Ok(None)
    }

    async fn goto_definition(&self, p: GotoDefinitionParams) -> Result<Option<GotoDefinitionResponse>> {
        let uri = p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        let Some((target, resolved, origin)) = self.target_at(&uri, pos).await else { return Ok(None) };
        let Some(file) = resolved.file().map(Path::to_path_buf) else { return Ok(None) };
        let Ok(dest) = Url::from_file_path(&file) else { return Ok(None) };
        let line = target
            .text
            .split_once('#')
            .and_then(|(_, frag)| {
                let text = std::fs::read_to_string(&file).ok()?;
                scan::find_anchor(&text, frag)
            })
            .unwrap_or(0) as u32;
        let at = Range::new(Position::new(line, 0), Position::new(line, 0));
        // A link with an origin range makes editors underline the whole target on
        // Cmd/Ctrl-hover instead of just the word under the mouse.
        Ok(Some(GotoDefinitionResponse::Link(vec![LocationLink {
            origin_selection_range: Some(origin),
            target_uri: dest,
            target_range: at,
            target_selection_range: at,
        }])))
    }

    async fn hover(&self, p: HoverParams) -> Result<Option<Hover>> {
        let uri = p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        let Some((_, resolved, range)) = self.target_at(&uri, pos).await else { return Ok(None) };
        let via_alias = matches!(resolved, Resolved::Alias(_));
        let value = match resolved {
            Resolved::Found(file) | Resolved::Alias(file) => {
                let state = self.state.read().await;
                let title = std::fs::read_to_string(&file).ok().and_then(|t| scan::document_title(&t));
                let origin = state
                    .catalog
                    .locate(&file)
                    .map(|c| {
                        let cv = &state.catalog.components[c.cv];
                        format!("{} · module `{}`", cv.display(), c.module)
                    })
                    .unwrap_or_default();
                let rel = state
                    .roots
                    .iter()
                    .find_map(|r| file.strip_prefix(catalog::canonical(r)).ok())
                    .unwrap_or(&file)
                    .display()
                    .to_string();
                let alias = if via_alias { "\n\n_via page alias_" } else { "" };
                match title {
                    Some(t) => format!("**{t}**\n\n{origin}\n\n`{rel}`{alias}"),
                    None => format!("{origin}\n\n`{rel}`{alias}"),
                }
            }
            Resolved::NotFound(msg) => format!("⚠️ {msg}"),
            Resolved::Skipped => return Ok(None),
        };
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::Markdown, value }),
            range: Some(range),
        }))
    }

    async fn references(&self, p: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = p.text_document_position.text_document.uri;
        let Some(resource) = self.subject(&uri, p.text_document_position.position).await else {
            return Ok(None);
        };
        let (catalog, open) = self.snapshot().await;
        let include_declaration = p.context.include_declaration;
        let locations = tokio::task::spawn_blocking(move || {
            let mut out: Vec<Location> = refs::usages(&catalog, &resource, &|f| text_of(&open, f))
                .into_iter()
                .filter_map(|u| {
                    let range = Range::new(Position::new(u.line as u32, u.start), Position::new(u.line as u32, u.end));
                    Some(Location::new(Url::from_file_path(&u.file).ok()?, range))
                })
                .collect();
            if include_declaration {
                out.extend(Url::from_file_path(&resource).ok().map(|u| Location::new(u, Range::default())));
            }
            out
        })
        .await
        .unwrap_or_default();
        Ok(Some(locations))
    }

    async fn prepare_rename(&self, p: TextDocumentPositionParams) -> Result<Option<PrepareRenameResponse>> {
        let Some((target, resolved, _)) = self.target_at(&p.text_document.uri, p.position).await else {
            return Ok(None);
        };
        let file = match resolved {
            Resolved::Found(f) => f,
            Resolved::Alias(_) => return Err(invalid("This reference uses a page alias; rename it from a direct reference")),
            _ => return Ok(None),
        };
        let state = self.state.read().await;
        let Some((_, _, relative)) = refs::family_relative(&state.catalog, &file) else {
            return Err(invalid("Only files inside a module's family directory can be renamed"));
        };
        let Some(line) = state.docs.get(&p.text_document.uri).and_then(|t| scan::lines(t).nth(target.line)) else {
            return Ok(None);
        };
        let (start, end) = refs::relative_span(&target);
        if line[start..end].contains('{') {
            return Err(invalid("This reference uses an attribute and can't be renamed from here"));
        }
        let n = target.line as u32;
        let range = Range::new(Position::new(n, scan::utf16_col(line, start)), Position::new(n, scan::utf16_col(line, end)));
        Ok(Some(PrepareRenameResponse::RangeWithPlaceholder { range, placeholder: relative }))
    }

    async fn rename(&self, p: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let pos = p.text_document_position;
        let Some((_, Resolved::Found(from), _)) = self.target_at(&pos.text_document.uri, pos.position).await else {
            return Err(invalid("Place the cursor on an xref, include or image target to rename it"));
        };
        if !self.state.read().await.can_rename_files {
            return Err(invalid("This editor can't rename files through the language server"));
        }
        let (catalog, open) = self.snapshot().await;
        let Some((dir, family, _)) = refs::family_relative(&catalog, &from) else {
            return Err(invalid("Only files inside a module's family directory can be renamed"));
        };
        let new_rel = refs::normalize_new_name(&p.new_name, family).map_err(invalid)?;
        let to = dir.join(&new_rel);
        if to == from {
            return Ok(None);
        }
        if to.exists() {
            return Err(invalid(format!("`{new_rel}` already exists")));
        }
        let moves = vec![(from.clone(), to.clone())];
        let plan = tokio::task::spawn_blocking(move || refs::plan_moves(&catalog, &moves, &|f| text_of(&open, f)))
            .await
            .map_err(|e| invalid(e.to_string()))?
            .map_err(invalid)?;
        self.warn_skipped(&plan.skipped).await;
        Ok(Self::workspace_edit(&plan, &[(from, to)]))
    }

    async fn will_rename_files(&self, p: RenameFilesParams) -> Result<Option<WorkspaceEdit>> {
        let (catalog, open) = self.snapshot().await;
        let mut moves = Vec::new();
        for r in &p.files {
            let (Ok(old), Ok(new)) = (Url::parse(&r.old_uri), Url::parse(&r.new_uri)) else { continue };
            let (Ok(old), Ok(new)) = (old.to_file_path(), new.to_file_path()) else { continue };
            let old = canonical(&old);
            if old.is_dir() {
                for f in catalog.resources_under(&old) {
                    let rest = f.strip_prefix(&old).unwrap_or(&f).to_path_buf();
                    moves.push((f, new.join(rest)));
                }
            } else if catalog.locate(&old).is_some_and(|c| c.family.is_some()) {
                moves.push((old, new));
            }
        }
        if moves.is_empty() {
            return Ok(None);
        }
        let plan = tokio::task::spawn_blocking(move || refs::plan_moves(&catalog, &moves, &|f| text_of(&open, f)))
            .await
            .map_err(|e| invalid(e.to_string()))?;
        match plan {
            Ok(plan) => {
                self.warn_skipped(&plan.skipped).await;
                Ok(Self::workspace_edit(&plan, &[]))
            }
            Err(msg) => {
                self.client.log_message(MessageType::INFO, format!("antora-ls: references not updated: {msg}")).await;
                Ok(None)
            }
        }
    }
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--version") {
        println!("antora-ls {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let (service, socket) = LspService::new(|client| Backend {
        client,
        state: Arc::new(RwLock::new(State::default())),
    });
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket).serve(service).await;
}

#[cfg(test)]
mod tests;
