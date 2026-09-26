mod catalog;
mod scan;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use catalog::{Catalog, MacroKind, Resolved};

#[derive(Default)]
struct State {
    roots: Vec<PathBuf>,
    catalog: Catalog,
    docs: HashMap<Url, String>,
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
        self.client
            .log_message(
                MessageType::INFO,
                format!("antora-ls: indexed {} component version(s)", catalog.components.len()),
            )
            .await;
        self.state.write().await.catalog = catalog;
        self.publish_all().await;
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

fn diagnostics(catalog: &Catalog, path: &Path, text: &str) -> Vec<Diagnostic> {
    let ctx = catalog.locate(path);
    let lines: Vec<&str> = scan::lines(text).collect();
    scan::scan(text)
        .into_iter()
        .filter_map(|t| match catalog.resolve(ctx.as_ref(), path, &t.text, t.kind) {
            Resolved::NotFound(msg) => {
                let line = lines[t.line];
                Some(Diagnostic {
                    range: Range::new(
                        Position::new(t.line as u32, scan::utf16_col(line, t.start)),
                        Position::new(t.line as u32, scan::utf16_col(line, t.end)),
                    ),
                    severity: Some(DiagnosticSeverity::WARNING),
                    source: Some("antora".into()),
                    message: msg,
                    ..Default::default()
                })
            }
            _ => None,
        })
        .collect()
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
        self.state.write().await.roots = roots;

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
                ..Default::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        let watchers = ["**/antora.yml", "**/modules/**"]
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
        if p.text_document.uri.path().ends_with("antora.yml") {
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
            c.typ != FileChangeType::CHANGED || c.uri.path().ends_with("antora.yml")
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
        let Some((target, Resolved::Found(file), _)) = self.target_at(&uri, pos).await else {
            return Ok(None);
        };
        let Ok(dest) = Url::from_file_path(&file) else { return Ok(None) };
        let line = target
            .text
            .split_once('#')
            .and_then(|(_, frag)| {
                let text = std::fs::read_to_string(&file).ok()?;
                scan::find_anchor(&text, frag)
            })
            .unwrap_or(0) as u32;
        let at = Position::new(line, 0);
        Ok(Some(GotoDefinitionResponse::Scalar(Location::new(dest, Range::new(at, at)))))
    }

    async fn hover(&self, p: HoverParams) -> Result<Option<Hover>> {
        let uri = p.text_document_position_params.text_document.uri;
        let pos = p.text_document_position_params.position;
        let Some((_, resolved, range)) = self.target_at(&uri, pos).await else { return Ok(None) };
        let value = match resolved {
            Resolved::Found(file) => {
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
                match title {
                    Some(t) => format!("**{t}**\n\n{origin}\n\n`{rel}`"),
                    None => format!("{origin}\n\n`{rel}`"),
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
