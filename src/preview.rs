//! Live site preview: runs `preview/antora-preview.js` with the project's Antora (under Bun
//! or Node) and forwards unsaved edits and cursor moves to it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

const SCRIPT: &str = include_str!("../preview/antora-preview.js");
const HANDSHAKE: &str = "ANTORA_PREVIEW ";
const STATUS: &str = "ANTORA_PREVIEW_STATUS ";
/// Keystrokes arriving within this window are sent as one update.
const COALESCE: Duration = Duration::from_millis(40);

pub enum Msg {
    /// New text for a file; `None` means "use what's on disk".
    Update(PathBuf, Option<String>),
    Cursor(PathBuf, u32),
}

/// Output of the preview server.
pub enum Event {
    Log(String),
    Status(Status),
    /// The server process ended by itself.
    Exited,
    /// The server was stopped with [`Preview::stop`].
    Stopped,
}

/// Build progress reported by the server.
#[derive(serde::Deserialize)]
pub struct Status {
    /// `building`, `ready` or `failed`.
    pub state: String,
    pub phase: Option<String>,
    pub error: Option<String>,
}

pub struct Preview {
    pub base: String,
    tx: mpsc::UnboundedSender<Msg>,
    child: Child,
    stopped: Arc<AtomicBool>,
}

impl Preview {
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Kill the server, e.g. to cancel a build.
    pub fn stop(mut self) {
        self.stopped.store(true, SeqCst);
        let _ = self.child.start_kill();
    }

    pub fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }

    /// URL that redirects to the page for `file` (or the site start page).
    pub fn open_url(&self, file: &Path) -> String {
        format!("{}/__preview/open?path={}", self.base, percent_encode(&file.to_string_lossy()))
    }

    /// Start the server for `playbook`. It answers right away and builds the site in the
    /// background; `on_event` receives its log lines and build progress.
    pub async fn start(
        runtime: &Path,
        playbook: &Path,
        on_event: impl Fn(Event) + Send + Sync + 'static,
    ) -> Result<Preview, String> {
        let dir = std::env::temp_dir().join(format!("antora-ls-{}", env!("CARGO_PKG_VERSION")));
        let script = dir.join("antora-preview.js");
        std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(&script, SCRIPT))
            .map_err(|e| format!("can't write {}: {e}", script.display()))?;

        let mut child = Command::new(runtime)
            .arg(&script)
            .arg(playbook)
            .current_dir(playbook.parent().unwrap_or(Path::new(".")))
            .stdin(Stdio::piped()) // the server exits when this closes
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("can't run {}: {e}", runtime.display()))?;

        let on_event = Arc::new(on_event);
        let log = {
            let on_event = on_event.clone();
            Arc::new(move |line: String| on_event(Event::Log(line)))
        };
        let stderr = child.stderr.take().expect("piped");
        let recent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        {
            let (log, recent) = (log.clone(), recent.clone());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(mut r) = recent.lock() {
                        r.push(line.clone());
                        let excess = r.len().saturating_sub(20);
                        r.drain(..excess);
                    }
                    log(line);
                }
            });
        }

        let mut stdout = BufReader::new(child.stdout.take().expect("piped")).lines();
        let handshake = tokio::time::timeout(Duration::from_secs(120), async {
            while let Ok(Some(line)) = stdout.next_line().await {
                if let Some(rest) = line.strip_prefix(HANDSHAKE) {
                    return Some(rest.to_string());
                }
                log(line);
            }
            None
        })
        .await;
        let failure = |what: &str| {
            let tail = recent.lock().map(|r| r.join("\n")).unwrap_or_default();
            format!("preview server {what}\n{tail}")
        };
        let rest = match handshake {
            Ok(Some(rest)) => rest,
            Ok(None) => return Err(failure("exited during startup")),
            Err(_) => return Err(failure("did not start within 120 s")),
        };
        let (base, token) = rest.split_once(' ').ok_or_else(|| failure("sent a malformed handshake"))?;
        let (base, token) = (base.to_string(), token.to_string());

        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_ = stopped.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = stdout.next_line().await {
                match line.strip_prefix(STATUS).map(serde_json::from_str::<Status>) {
                    Some(Ok(status)) => on_event(Event::Status(status)),
                    _ => on_event(Event::Log(line)),
                }
            }
            on_event(if stopped_.load(SeqCst) { Event::Stopped } else { Event::Exited });
        });

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(forward(base.clone(), token, rx, log.clone()));
        Ok(Preview { base, tx, child, stopped })
    }
}

/// Send messages to the server, keeping only the latest text per file within each burst.
async fn forward(
    base: String,
    token: String,
    mut rx: mpsc::UnboundedReceiver<Msg>,
    log: Arc<impl Fn(String) + Send + Sync + 'static>,
) {
    while let Some(first) = rx.recv().await {
        let mut updates: HashMap<PathBuf, Option<String>> = HashMap::new();
        let mut order: Vec<PathBuf> = Vec::new();
        let mut cursor = None;
        let mut take = |msg: Msg| match msg {
            Msg::Update(p, text) => {
                if updates.insert(p.clone(), text).is_none() {
                    order.push(p);
                }
            }
            Msg::Cursor(p, line) => cursor = Some((p, line)),
        };
        take(first);
        tokio::time::sleep(COALESCE).await;
        while let Ok(msg) = rx.try_recv() {
            take(msg);
        }
        for path in order {
            let text = updates.remove(&path).flatten();
            let body = serde_json::json!({ "path": path, "text": text });
            if let Err(e) = post(&base, &token, "update", &body).await {
                log(format!("[antora-ls] preview update failed: {e}"));
            }
        }
        if let Some((path, line)) = cursor {
            let _ = post(&base, &token, "cursor", &serde_json::json!({ "path": path, "line": line })).await;
        }
    }
}

/// Minimal HTTP/1.1 POST to the local preview server.
async fn post(base: &str, token: &str, route: &str, body: &serde_json::Value) -> Result<(), String> {
    let host = base.strip_prefix("http://").ok_or("bad base url")?;
    let mut stream = tokio::net::TcpStream::connect(host).await.map_err(|e| e.to_string())?;
    let body = body.to_string();
    let request = format!(
        "POST /__preview/{route} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n\
         x-preview-token: {token}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.map_err(|e| e.to_string())?;
    let status = String::from_utf8_lossy(&response);
    let status = status.lines().next().unwrap_or("");
    if status.contains(" 2") { Ok(()) } else { Err(status.to_string()) }
}

/// Bun if available, else Node: an explicit setting, then PATH, then Bun's default install dir.
pub fn find_runtime(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(c) = configured {
        return Some(PathBuf::from(c));
    }
    // Windows usually has no HOME; Bun installs to %USERPROFILE%\.bun there.
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    runtime_in(std::env::var_os("PATH"), home.map(PathBuf::from), std::env::consts::EXE_SUFFIX)
}

/// [`find_runtime`] without a setting, given the PATH, the home directory and the suffix of
/// executables (`.exe` on Windows).
pub(crate) fn runtime_in(path: Option<OsString>, home: Option<PathBuf>, exe_suffix: &str) -> Option<PathBuf> {
    let exe = |name: &str| format!("{name}{exe_suffix}");
    let on_path = |name: &str| {
        path.iter()
            .flat_map(|p| std::env::split_paths(p).collect::<Vec<_>>())
            .map(|d| d.join(exe(name)))
            .find(|p| p.is_file())
    };
    let home_bun = home.map(|h| h.join(".bun").join("bin").join(exe("bun"))).filter(|p| p.is_file());
    on_path("bun").or(home_bun).or_else(|| on_path("node"))
}

/// The playbook to preview: `antora-playbook.yml` if present, else the first one found.
pub fn pick_playbook(playbooks: &[PathBuf]) -> Option<PathBuf> {
    playbooks
        .iter()
        .find(|p| p.file_name().is_some_and(|n| n == "antora-playbook.yml"))
        .or_else(|| playbooks.first())
        .cloned()
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
