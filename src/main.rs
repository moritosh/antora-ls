mod catalog;
mod markdown;
mod nav;
mod preview;
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
    /// `preview.runtime` from initialization options: path to bun or node.
    preview_runtime: Option<String>,
}

const CMD_SITE_PREVIEW: &str = "antora.preview.site";
const CMD_MARKDOWN_PREVIEW: &str = "antora.preview.markdown";

/// The page shown in the Markdown preview. There is one preview; it follows the page being
/// edited, so it only has to be opened once.
type MarkdownSource = Arc<std::sync::Mutex<Option<PathBuf>>>;

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
    preview: std::sync::Mutex<Option<preview::Preview>>,
    preview_starting: std::sync::atomic::AtomicBool,
    markdown: MarkdownSource,
    markdown_generation: Arc<std::sync::atomic::AtomicU64>,
}

fn snapshot_of(state: &State) -> (Arc<Catalog>, OpenTexts) {
    let open = state
        .docs
        .iter()
        .filter_map(|(uri, text)| Some((canonical(&uri.to_file_path().ok()?), text.clone())))
        .collect();
    (state.catalog.clone(), open)
}

/// The Markdown rendering of the followed page: a read-only file in the temp dir, one per
/// workspace and stable across server restarts so an open preview keeps updating.
fn markdown_path(roots: &[PathBuf]) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    roots.hash(&mut h);
    let dir = std::env::temp_dir().join(format!("antora-ls-preview-{:016x}", h.finish()));
    let path = dir.join("Antora Preview.md");
    // Canonical, so image paths computed relative to it match what the editor sees.
    catalog::canonical_lenient(&path)
}

fn write_markdown(catalog: &Catalog, open: &OpenTexts, source: &Path, dest: &Path) -> std::io::Result<()> {
    let text = text_of(open, source).unwrap_or_default();
    let md = markdown::render(catalog, source, &text, &|p| text_of(open, p), dest.parent());
    if std::fs::read_to_string(dest).is_ok_and(|old| old == md) {
        return Ok(());
    }
    std::fs::create_dir_all(dest.parent().unwrap_or(Path::new(".")))?;
    // Write a sibling and rename over the old file, so the editor never reads a partial file;
    // read-only permissions keep accidental edits from blocking reloads.
    let tmp = dest.with_extension("md.tmp");
    std::fs::write(&tmp, md)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444))?;
    }
    std::fs::rename(&tmp, dest)
}

/// Regenerate the Markdown preview from the followed page (and anything it includes).
async fn refresh_markdown(state: Arc<RwLock<State>>, source: MarkdownSource) {
    let Some(source) = source.lock().unwrap().clone() else { return };
    let (catalog, open, dest) = {
        let state = state.read().await;
        let (catalog, open) = snapshot_of(&state);
        (catalog, open, markdown_path(&state.roots))
    };
    let _ = tokio::task::spawn_blocking(move || write_markdown(&catalog, &open, &source, &dest)).await;
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
        snapshot_of(&*self.state.read().await)
    }

    /// Convert `file` to Markdown and open the result for the editor's Markdown preview.
    async fn open_markdown_preview(&self, file: PathBuf) -> std::result::Result<(), String> {
        let (catalog, open) = self.snapshot().await;
        let dest = markdown_path(&self.state.read().await.roots);
        let (src, out) = (file.clone(), dest.clone());
        tokio::task::spawn_blocking(move || write_markdown(&catalog, &open, &src, &out))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("can't write {}: {e}", dest.display()))?;
        let first = self.markdown.lock().unwrap().replace(file).is_none();
        let uri = Url::from_file_path(&dest).map_err(|_| "bad path".to_string())?;
        let params = ShowDocumentParams { uri, external: Some(false), take_focus: Some(true), selection: None };
        self.client.show_document(params).await.map_err(|e| e.to_string())?;
        if first {
            self.client
                .show_message(
                    MessageType::INFO,
                    "Antora: run \"markdown: open preview to the side\" (cmd-k v) in the opened tab. \
                     The preview then follows whichever page you edit.",
                )
                .await;
        }
        Ok(())
    }

    /// Point the Markdown preview at `path` when the user works in another page.
    async fn follow_markdown(&self, path: &Path) {
        let current = self.markdown.lock().unwrap().clone();
        let Some(current) = current else { return };
        if current == path || !catalog::is_adoc(path) || self.state.read().await.catalog.locate(path).is_none() {
            return;
        }
        *self.markdown.lock().unwrap() = Some(path.to_path_buf());
        self.schedule_markdown_refresh();
    }

    /// Regenerate the Markdown preview shortly after the last keystroke.
    fn schedule_markdown_refresh(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        if self.markdown.lock().unwrap().is_none() {
            return;
        }
        let generation = self.markdown_generation.fetch_add(1, SeqCst) + 1;
        let (counter, state, previews) = (self.markdown_generation.clone(), self.state.clone(), self.markdown.clone());
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            if counter.load(SeqCst) == generation {
                refresh_markdown(state, previews).await;
            }
        });
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

    /// Forward to the live preview, if one is running.
    fn to_preview(&self, msg: preview::Msg) {
        if let Some(p) = self.preview.lock().unwrap().as_ref() {
            p.send(msg);
        }
    }

    /// Start the live site preview if needed and open `file`'s page in the browser.
    async fn open_site_preview(&self, file: PathBuf) -> std::result::Result<(), String> {
        let running = {
            let mut guard = self.preview.lock().unwrap();
            if guard.as_mut().is_some_and(|p| !p.is_running()) {
                *guard = None; // exited or crashed: start a new one
            }
            guard.as_ref().map(|p| p.open_url(&file))
        };
        let url = match running {
            Some(url) => url,
            None => {
                use std::sync::atomic::Ordering::SeqCst;
                if self.preview_starting.swap(true, SeqCst) {
                    return Err("The preview is already starting".into());
                }
                let started = self.start_preview().await;
                self.preview_starting.store(false, SeqCst);
                let p = started?;
                let url = p.open_url(&file);
                *self.preview.lock().unwrap() = Some(p);
                url
            }
        };
        let uri = Url::parse(&url).map_err(|e| e.to_string())?;
        let params = ShowDocumentParams { uri, external: Some(true), take_focus: Some(true), selection: None };
        self.client.show_document(params).await.map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn start_preview(&self) -> std::result::Result<preview::Preview, String> {
        let (catalog, open) = self.snapshot().await;
        let playbook = preview::pick_playbook(&catalog.playbooks)
            .ok_or("No Antora playbook (*playbook*.yml with content sources) found in the workspace")?;
        let runtime = preview::find_runtime(self.state.read().await.preview_runtime.as_deref()).ok_or(
            "Neither bun nor node was found. Set lsp.antora-ls.initialization_options.preview.runtime in Zed settings",
        )?;
        self.client
            .log_message(MessageType::INFO, format!("antora-ls: starting preview of {} with {}", playbook.display(), runtime.display()))
            .await;
        let client = self.client.clone();
        let log = move |line: String| {
            let client = client.clone();
            tokio::spawn(async move { client.log_message(MessageType::LOG, line).await });
        };
        let p = preview::Preview::start(&runtime, &playbook, log).await?;
        // Show unsaved edits right away.
        for (path, text) in open {
            p.send(preview::Msg::Update(path, Some(text)));
        }
        self.client.log_message(MessageType::INFO, format!("antora-ls: preview running at {}", p.base)).await;
        Ok(p)
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
        let preview_runtime = params
            .initialization_options
            .as_ref()
            .and_then(|o| o.pointer("/preview/runtime"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        {
            let mut state = self.state.write().await;
            state.roots = roots;
            state.can_rename_files = can_rename_files;
            state.preview_runtime = preview_runtime;
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
                // Editors ask for highlights whenever the cursor moves; the preview follows along.
                document_highlight_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![CMD_SITE_PREVIEW.into(), CMD_MARKDOWN_PREVIEW.into()],
                    work_done_progress_options: Default::default(),
                }),
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
        if let Ok(path) = uri.to_file_path() {
            self.follow_markdown(&canonical(&path)).await;
        }
        self.publish(&uri).await;
    }

    async fn did_change(&self, p: DidChangeTextDocumentParams) {
        let uri = p.text_document.uri;
        if let Some(change) = p.content_changes.into_iter().last() {
            if let Ok(path) = uri.to_file_path() {
                self.to_preview(preview::Msg::Update(canonical(&path), Some(change.text.clone())));
            }
            self.state.write().await.docs.insert(uri.clone(), change.text);
        }
        if let Ok(path) = uri.to_file_path() {
            self.follow_markdown(&canonical(&path)).await;
        }
        self.schedule_markdown_refresh();
        self.publish(&uri).await;
    }

    async fn did_save(&self, p: DidSaveTextDocumentParams) {
        let uri = &p.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            self.to_preview(preview::Msg::Update(canonical(&path), None)); // disk is current again
        }
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
        if let Ok(path) = p.text_document.uri.to_file_path() {
            self.to_preview(preview::Msg::Update(canonical(&path), None)); // drop unsaved edits
        }
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

    async fn document_highlight(&self, p: DocumentHighlightParams) -> Result<Option<Vec<DocumentHighlight>>> {
        let pos = p.text_document_position_params;
        if let Ok(path) = pos.text_document.uri.to_file_path() {
            let path = canonical(&path);
            self.to_preview(preview::Msg::Cursor(path.clone(), pos.position.line + 1));
            self.follow_markdown(&path).await;
        }
        Ok(None)
    }

    async fn code_action(&self, p: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let Ok(path) = p.text_document.uri.to_file_path() else { return Ok(None) };
        if self.state.read().await.catalog.locate(&canonical(&path)).is_none() {
            return Ok(None);
        }
        let args = vec![serde_json::Value::String(p.text_document.uri.to_string())];
        let action = |title: &str, command: &str| {
            CodeActionOrCommand::CodeAction(CodeAction {
                title: title.into(),
                kind: Some(CodeActionKind::EMPTY),
                command: Some(Command { title: title.into(), command: command.into(), arguments: Some(args.clone()) }),
                ..Default::default()
            })
        };
        Ok(Some(vec![
            action("Antora: Open live site preview", CMD_SITE_PREVIEW),
            action("Antora: Open Markdown preview", CMD_MARKDOWN_PREVIEW),
        ]))
    }

    async fn execute_command(&self, p: ExecuteCommandParams) -> Result<Option<serde_json::Value>> {
        let file = p
            .arguments
            .first()
            .and_then(|a| a.as_str())
            .and_then(|u| Url::parse(u).ok())
            .and_then(|u| u.to_file_path().ok())
            .map(|p| canonical(&p))
            .ok_or_else(|| invalid("Expected a document URI argument"))?;
        match p.command.as_str() {
            CMD_SITE_PREVIEW => {
                if let Err(msg) = self.open_site_preview(file).await {
                    self.client.show_message(MessageType::ERROR, format!("Antora preview: {msg}")).await;
                }
            }
            CMD_MARKDOWN_PREVIEW => {
                if let Err(msg) = self.open_markdown_preview(file).await {
                    self.client.show_message(MessageType::ERROR, format!("Antora Markdown preview: {msg}")).await;
                }
            }
            other => return Err(invalid(format!("Unknown command {other}"))),
        }
        Ok(None)
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
        preview: Default::default(),
        preview_starting: Default::default(),
        markdown: Default::default(),
        markdown_generation: Default::default(),
    });
    Server::new(tokio::io::stdin(), tokio::io::stdout(), socket).serve(service).await;
}

#[cfg(test)]
mod tests;
