//! The language server manager: one slot per configured server, per-query
//! suffix routing, lazy starts, document synchronization, and the bounded
//! relaunch a crashed server earns.
//!
//! Registering starts nothing: a session pays zero processes until a routed
//! file is asked about. The first ask launches behind one lock, so
//! concurrent asks share one handshake, and every opened document carries a
//! strictly increasing version.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use houyicoder_api::launcher::LauncherChild;
use houyicoder_api::launcher::ProcessLauncher;
use houyicoder_api::launcher::SpawnPolicy;
use houyicoder_api::launcher::SpawnRequest;
use houyicoder_api::launcher::StdioConfig;
use houyicoder_api::launcher::StdioMode;
use houyicoder_config as config;
use houyicoder_protocol::lsp::LspDiagnostic;
use houyicoder_protocol::lsp::LspPublishDiagnostics;
use houyicoder_protocol::lsp::LspServerState;
use houyicoder_protocol::lsp::LspTransportKind;
use houyicoder_protocol::lsp::ScopedLspServerConfig;
use serde_json::Value;
#[cfg(test)]
use tokio::io::DuplexStream;
use tokio::sync::Notify;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio::time::timeout;

use super::client::DEFAULT_RETRY_DELAYS;
use super::client::LspClient;
use super::client::RetryDelays;
use super::client::ServerNotification;
use super::client::initialize_params;
use crate::child_process::Pipes;
use crate::child_process::StderrTail;
use crate::child_process::child_environment;
use crate::child_process::file_uri;
use crate::child_process::host_environment;
use crate::child_process::start;

/// The windows and cadences one manager runs with. Production uses the
/// defaults; a test injects millisecond values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspTimings {
    /// The retry schedule after a transient request error.
    pub retry_delays: RetryDelays,
    /// The first relaunch delay after a crash; later attempts double it.
    pub restart_base: Duration,
    /// How long a write waits for a fresh diagnostics publish before
    /// reporting none.
    pub diagnostics_grace: Duration,
    /// The longest that wait may run when the publish is slow to arrive.
    pub diagnostics_cap: Duration,
    /// How long a query waits for a server still reporting indexing before
    /// its answer is marked partial.
    pub index_wait: Duration,
    /// How often a waiting ask re-reads the indexing state.
    pub index_poll: Duration,
    /// How long a connection must stay up before its crash count resets.
    pub stable_after: Duration,
}

impl Default for LspTimings {
    fn default() -> Self {
        Self {
            retry_delays: DEFAULT_RETRY_DELAYS,
            restart_base: Duration::from_millis(500),
            diagnostics_grace: Duration::from_millis(150),
            diagnostics_cap: Duration::from_millis(500),
            index_wait: Duration::from_secs(3),
            index_poll: Duration::from_millis(50),
            stable_after: Duration::from_secs(30),
        }
    }
}

/// One live connection and the documents opened on it.
struct Connection {
    client: Arc<LspClient>,
    child: Option<LauncherChild>,
    /// Document URI to the version last sent; every change increments it.
    open_docs: HashMap<String, i32>,
    started_at: Instant,
}

/// The findings one publish left for one file, with the generation that
/// publish produced. An empty publish keeps its entry, because a file that
/// was checked and found clean is a fact a waiter may return; only a
/// connection that ends clears the store.
#[derive(Clone)]
struct PublishedDiagnostics {
    generation: u64,
    findings: Vec<LspDiagnostic>,
}

/// One configured server.
struct ServerSlot {
    config: ScopedLspServerConfig,
    /// True when an untrusted workspace holds a project entry off.
    locked: bool,
    enabled: AtomicBool,
    /// Coarse lifecycle; indexing overlays on read.
    state: RwLock<LspServerState>,
    /// Serializes first launches so concurrent asks share one handshake;
    /// held only by a launcher, never by a reader.
    start_gate: tokio::sync::Mutex<()>,
    live: tokio::sync::Mutex<Option<Connection>>,
    diagnostics: Mutex<HashMap<String, PublishedDiagnostics>>,
    /// Wakes the waiters watching a file's diagnostics.
    diag_notify: Notify,
    stderr: Mutex<StderrTail>,
    restarts: AtomicU32,
}

/// The manager itself. Cloning shares the slots, so every handle sees one
/// state.
#[derive(Clone)]
pub struct LspConnections {
    launcher: Arc<dyn ProcessLauncher>,
    workspace_root: Arc<Path>,
    timings: LspTimings,
    slots: Arc<RwLock<BTreeMap<String, Arc<ServerSlot>>>>,
    /// Set by shutdown; no launch may follow it, so a watcher waking from a
    /// backoff sleep cannot resurrect a server the session ended.
    closed: Arc<AtomicBool>,
}

impl LspConnections {
    /// A manager over one launcher and one workspace, with the given
    /// windows. Nothing is launched here.
    pub fn new(
        launcher: Arc<dyn ProcessLauncher>,
        workspace_root: Arc<Path>,
        timings: LspTimings,
    ) -> Self {
        Self {
            launcher,
            workspace_root,
            timings,
            slots: Arc::new(RwLock::new(BTreeMap::new())),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Register every resolved server. Disabled entries are recorded and never
    /// launched. No process starts here: the launch waits for the first ask,
    /// and the route a suffix takes is decided per query, so a switch or a
    /// re-registered entry changes it without a rebuild.
    pub async fn start_all(&self, servers: Vec<ScopedLspServerConfig>) {
        let mut slots = self.slots.write().await;
        for config in servers {
            if config.transport != LspTransportKind::Stdio {
                tracing::warn!(
                    server = %config.name,
                    "a language server entry names a transport this host does not launch; it stays idle"
                );
            }
            let name = config.name.clone();
            let locked = config.locked;
            let state = if config.disabled {
                LspServerState::Disabled {
                    reason: if locked {
                        "the workspace is not trusted".into()
                    } else {
                        "disabled by configuration".into()
                    },
                }
            } else {
                LspServerState::Idle
            };
            slots.insert(
                name,
                Arc::new(ServerSlot {
                    enabled: AtomicBool::new(!config.disabled),
                    config,
                    locked,
                    state: RwLock::new(state),
                    start_gate: tokio::sync::Mutex::new(()),
                    live: tokio::sync::Mutex::new(None),
                    diagnostics: Mutex::new(HashMap::new()),
                    diag_notify: Notify::new(),
                    stderr: Mutex::new(StderrTail::default()),
                    restarts: AtomicU32::new(0),
                }),
            );
        }
    }

    /// One path resolved against the workspace when it is relative, so the
    /// file URI and the server's view agree.
    pub(crate) fn resolve_path(&self, path_text: &str) -> PathBuf {
        let given = Path::new(path_text);
        if given.is_absolute() {
            given.to_path_buf()
        } else {
            self.workspace_root.join(given)
        }
    }

    /// Start one server if it is not running, or return the live connection.
    /// The start gate spans the handshake so concurrent asks share one
    /// launch, while readers of the live slot never wait on it: a status
    /// read during a slow handshake sees Starting and returns.
    pub(crate) async fn ensure_started(
        self: &Arc<Self>,
        name: &str,
    ) -> Result<Arc<LspClient>, String> {
        let slot = self
            .slots
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no language server named {name} is configured"))?;
        if let Some(client) = Self::current_client(&slot).await {
            return Ok(client);
        }
        let _gate = slot.start_gate.lock().await;
        if let Some(client) = Self::current_client(&slot).await {
            return Ok(client);
        }
        if self.closed.load(Ordering::Relaxed) {
            return Err("the language server manager is shut down".to_string());
        }
        if !slot.enabled.load(Ordering::Relaxed) {
            return Err(format!("{name} is disabled"));
        }
        if slot.config.transport != LspTransportKind::Stdio {
            return Err(format!(
                "{name} names a transport this host does not launch"
            ));
        }
        *slot.state.write().await = LspServerState::Starting;
        // A caller cancelled mid-handshake (its own timeout fired) must not
        // leave the slot reading Starting with nothing behind it.
        let mut guard = watch::StateGuard {
            slot: Arc::clone(&slot),
            armed: true,
        };
        let outcome = self.launch(&slot).await;
        guard.armed = false;
        match outcome {
            Ok(client) => {
                // A shutdown that landed mid-handshake wins: the session
                // asked for no servers, so the fresh connection is closed
                // rather than stored.
                if self.closed.load(Ordering::Relaxed) {
                    client
                        .0
                        .shutdown(Duration::from_millis(slot.config.shutdown_timeout_ms))
                        .await;
                    drop(client.1.child);
                    *slot.state.write().await = LspServerState::Idle;
                    return Err("the language server manager is shut down".to_string());
                }
                let (launched, connection) = client;
                // A switch turned off mid-handshake wins the same way a
                // shutdown does: the store decision and the switch's take
                // share the live lock, so a fresh connection cannot be
                // stored after a disable already looked and found none.
                let mut pending = Some(connection);
                let accepted = {
                    let mut live = slot.live.lock().await;
                    if slot.enabled.load(Ordering::Relaxed) {
                        *live = pending.take();
                        true
                    } else {
                        false
                    }
                };
                if !accepted {
                    if let Some(connection) = pending {
                        launched
                            .shutdown(Duration::from_millis(slot.config.shutdown_timeout_ms))
                            .await;
                        drop(connection.child);
                    }
                    *slot.state.write().await = LspServerState::Disabled {
                        reason: "disabled by the user".into(),
                    };
                    return Err(format!("{name} is disabled"));
                }
                *slot.state.write().await = LspServerState::Running;
                self.spawn_watcher(&slot, Arc::clone(&launched));
                Ok(launched)
            }
            Err(error) => {
                *slot.state.write().await = LspServerState::Failed {
                    error: error.clone(),
                };
                Err(error)
            }
        }
    }

    /// The live client of one slot, when one exists.
    async fn current_client(slot: &Arc<ServerSlot>) -> Option<Arc<LspClient>> {
        slot.live
            .lock()
            .await
            .as_ref()
            .map(|connection| Arc::clone(&connection.client))
    }

    /// Launch one server and run its handshake, returning the client and
    /// the connection the caller stores.
    async fn launch(
        self: &Arc<Self>,
        slot: &Arc<ServerSlot>,
    ) -> Result<(Arc<LspClient>, Connection), String> {
        let config = &slot.config;
        let workspace: PathBuf = config
            .workspace_folder
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.workspace_root.to_path_buf());
        let mut request = SpawnRequest::new(config.command.clone())
            .with_args(config.args.clone())
            .with_workspace(workspace.clone())
            .with_env(child_environment(&config.env, &host_environment()))
            .interactive();
        request.stdio = StdioConfig::piped_output();
        request.stdio.stdin = StdioMode::Piped;
        let mut child = self
            .launcher
            .spawn(request, SpawnPolicy::default().sandboxed())
            .map_err(|error| format!("the server did not start: {error}"))?;
        let pipes = child
            .take_pipes()
            .ok_or_else(|| "the launcher returned no pipes".to_string())?;
        let (Some(stdin), Some(stdout)) = (pipes.stdin, pipes.stdout) else {
            return Err("the launcher returned an incomplete pipe pair".into());
        };
        let (bridge, stderr) = start(Pipes {
            stdin,
            stdout,
            stderr: pipes.stderr,
        });
        if let Ok(mut tail) = slot.stderr.lock() {
            *tail = stderr;
        }
        let (client, notices) =
            LspClient::spawn(bridge.reader, bridge.writer, self.timings.retry_delays);
        let params = initialize_params(&workspace, config.initialization_options.as_ref());
        let budget = Duration::from_millis(config.startup_timeout_ms);
        client.initialize(params, budget).await?;
        if let Some(settings) = &config.settings
            && let Err(error) = client.notify(
                "workspace/didChangeConfiguration",
                serde_json::json!({"settings": settings}),
            )
        {
            tracing::warn!(
                server = %config.name,
                %error,
                "the server's settings could not be sent"
            );
        }
        spawn_drain(Arc::clone(slot), notices);
        let connection = Connection {
            client: Arc::clone(&client),
            child: Some(child),
            open_docs: HashMap::new(),
            started_at: Instant::now(),
        };
        Ok((client, connection))
    }

    /// Watch one connection: when it ends on its own, mark the slot and
    /// relaunch within the slot's configured bounds.
    fn spawn_watcher(self: &Arc<Self>, slot: &Arc<ServerSlot>, client: Arc<LspClient>) {
        let manager = self.clone();
        let slot = Arc::clone(slot);
        tokio::spawn(async move {
            client.wait_closed().await;
            let mut live = slot.live.lock().await;
            let Some(connection) = live.as_ref() else {
                return;
            };
            if !Arc::ptr_eq(&connection.client, &client) {
                return;
            }
            let stable = connection.started_at.elapsed() >= manager.timings.stable_after;
            live.take();
            drop(live);
            clear_diagnostics(&slot);
            if manager.closed.load(Ordering::Relaxed) {
                return;
            }
            if stable {
                slot.restarts.store(0, Ordering::Relaxed);
            }
            if !slot.enabled.load(Ordering::Relaxed) || !slot.config.restart_on_crash {
                if slot.enabled.load(Ordering::Relaxed) {
                    *slot.state.write().await = LspServerState::Failed {
                        error: "the server exited".into(),
                    };
                }
                return;
            }
            let attempt = slot.restarts.fetch_add(1, Ordering::Relaxed) + 1;
            if attempt > slot.config.max_restarts {
                *slot.state.write().await = LspServerState::Failed {
                    error: "the server kept exiting".into(),
                };
                return;
            }
            *slot.state.write().await = LspServerState::Failed {
                error: "the server exited; relaunching".into(),
            };
            // Each later attempt doubles the wait, bounded so an exponent
            // cannot overflow the base.
            let factor = 1u32 << (attempt - 1).min(5);
            sleep(manager.timings.restart_base * factor).await;
            if manager.closed.load(Ordering::Relaxed) {
                return;
            }
            drop(manager.ensure_started(&slot.config.name).await);
        });
    }

    /// Whether a server is still indexing after a short wait, so a query
    /// answer can say its results may be partial.
    pub async fn indexing_after_wait(&self, name: &str) -> bool {
        self.wait_indexing_briefly(name, self.timings.index_wait)
            .await
    }

    /// Wait briefly for a server that reports indexing to settle. True means
    /// it is still indexing when the budget runs out; the caller marks its
    /// answer as possibly partial.
    pub async fn wait_indexing_briefly(&self, name: &str, budget: Duration) -> bool {
        let Some(slot) = self.slots.read().await.get(name).cloned() else {
            return false;
        };
        let deadline = Instant::now() + budget;
        loop {
            let live = slot.live.lock().await;
            let indexing = live
                .as_ref()
                .is_some_and(|connection| connection.client.is_indexing());
            drop(live);
            if !indexing {
                return false;
            }
            if Instant::now() >= deadline {
                return true;
            }
            sleep(self.timings.index_poll).await;
        }
    }

    /// Synchronize one changed file with the server that owns its suffix:
    /// open it with version one the first time, then send a change with the
    /// next version. Nothing is announced as saved, so a read-only query on
    /// the file does not trigger the server's check-on-save work.
    pub async fn sync_file_content(
        self: &Arc<Self>,
        path: &Path,
        content: &str,
    ) -> Result<Option<String>, String> {
        self.sync_content(path, content, false).await
    }

    /// Synchronize one just-written file and announce the save, so a
    /// check-on-save server runs its diagnostics for the new content. The
    /// read decorator waits for that publish; a query path uses
    /// sync_file_content and never announces a save it did not make.
    pub async fn sync_file_saved(
        self: &Arc<Self>,
        path: &Path,
        content: &str,
    ) -> Result<Option<String>, String> {
        self.sync_content(path, content, true).await
    }

    /// The shared body of the two syncs. The notification goes through the
    /// connection held under this lock, and the version is recorded only
    /// after the frame was accepted, so a replaced connection cannot claim a
    /// delivery it never made. None means no server routes the suffix; an
    /// error names a routed server whose connection went away between the
    /// start and the send.
    async fn sync_content(
        self: &Arc<Self>,
        path: &Path,
        content: &str,
        announce_save: bool,
    ) -> Result<Option<String>, String> {
        let Some(name) = self.server_for_path(path).await else {
            return Ok(None);
        };
        drop(self.ensure_started(&name).await?);
        let slot = self
            .slots
            .read()
            .await
            .get(&name)
            .cloned()
            .ok_or_else(|| format!("no language server named {name} is configured"))?;
        let extension = watch::extension_of(path);
        let language = config::language_for_extension(&slot.config, &extension)
            .unwrap_or("plaintext")
            .to_string();
        let uri = file_uri(path);
        let mut live = slot.live.lock().await;
        let connection = live
            .as_mut()
            .ok_or_else(|| format!("{name} is not connected"))?;
        match connection.open_docs.get(&uri).copied() {
            Some(version) => {
                let next = version + 1;
                connection.client.notify(
                    "textDocument/didChange",
                    serde_json::json!({
                        "textDocument": {"uri": uri.clone(), "version": next},
                        "contentChanges": [{"text": content}],
                    }),
                )?;
                connection.open_docs.insert(uri.clone(), next);
            }
            None => {
                connection.client.notify(
                    "textDocument/didOpen",
                    serde_json::json!({
                        "textDocument": {
                            "uri": uri.clone(),
                            "languageId": language,
                            "version": 1,
                            "text": content,
                        },
                    }),
                )?;
                connection.open_docs.insert(uri.clone(), 1);
            }
        }
        if announce_save {
            connection.client.notify(
                "textDocument/didSave",
                serde_json::json!({"textDocument": {"uri": uri}}),
            )?;
        }
        Ok(Some(name))
    }

    /// Ask one file-scoped question: route the suffix, start its server,
    /// synchronize the given content without announcing a save, and forward
    /// the request. The caller opens the document through ensure_document;
    /// this path drains a dropped connection instead of claiming a delivery.
    pub async fn request_for_file(
        self: &Arc<Self>,
        path: &Path,
        method: &str,
        params: Value,
        content: Option<&str>,
        budget: Duration,
    ) -> Result<Value, String> {
        let name = match self.server_for_path(path).await {
            Some(name) => name,
            None => return Err(format!("no language server routes {}", path.display())),
        };
        if let Some(content) = content {
            self.sync_file_content(path, content).await?;
        }
        // The request addresses the connection live at this instant; a
        // swap between the open and the ask is a crash the request reports.
        let Some(slot) = self.slots.read().await.get(&name).cloned() else {
            return Err(format!("no language server named {name} is configured"));
        };
        let Some(client) = Self::current_client(&slot).await else {
            return Err(format!("{name} is not connected"));
        };
        let started = Instant::now();
        let outcome = timeout(budget, client.request(method, params)).await;
        tracing::debug!(
            server = %name,
            method,
            elapsed_ms = started.elapsed().as_millis() as u64,
            answered = outcome.is_ok(),
            "the language server request settled"
        );
        outcome
            .map_err(|_| "the language server did not answer inside its budget".to_string())?
            .map_err(|error| error.to_string())
    }
}

/// Drop the diagnostics a previous connection published: its findings
/// describe a server that is no longer running, and keeping them would
/// serve stale errors as current.
fn clear_diagnostics(slot: &ServerSlot) {
    if let Ok(mut map) = slot.diagnostics.lock() {
        map.clear();
    }
    slot.diag_notify.notify_waiters();
}

/// Drain one connection's notifications into its slot's diagnostic store
/// until the stream ends. An empty publish keeps the file's entry with no
/// findings: the server checked the file and found it clean, which is what
/// lets a waiter report "no problems" rather than "no publish".
fn spawn_drain(slot: Arc<ServerSlot>, mut notices: mpsc::UnboundedReceiver<ServerNotification>) {
    tokio::spawn(async move {
        while let Some(notification) = notices.recv().await {
            if notification.method != "textDocument/publishDiagnostics" {
                continue;
            }
            let Ok(params) = serde_json::from_value::<LspPublishDiagnostics>(notification.params)
            else {
                tracing::warn!("a diagnostics notification did not parse; ignored");
                continue;
            };
            // The key resolves a path on disk, which is not work to hold a
            // std mutex for: compute it before the lock.
            let key = diagnostic_key(&params.uri);
            if let Ok(mut map) = slot.diagnostics.lock() {
                let entry = map.entry(key).or_insert_with(|| PublishedDiagnostics {
                    generation: 0,
                    findings: Vec::new(),
                });
                entry.generation += 1;
                entry.findings = params.diagnostics;
            }
            slot.diag_notify.notify_waiters();
        }
    });
}

impl LspConnections {
    /// Attach an in-memory peer as one server's live connection, so a test
    /// drives the manager's rules without a launcher. The peer answers the
    /// handshake; the caller keeps its other half.
    #[cfg(test)]
    pub(crate) async fn attach_for_test(
        self: &Arc<Self>,
        name: &str,
        reader: DuplexStream,
        writer: DuplexStream,
    ) -> Result<Arc<LspClient>, String> {
        let slot = self
            .slots
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no language server named {name} is configured"))?;
        let (client, notices) = LspClient::spawn(reader, writer, self.timings.retry_delays);
        let params = initialize_params(
            &self.workspace_root,
            slot.config.initialization_options.as_ref(),
        );
        client
            .initialize(
                params,
                Duration::from_millis(slot.config.startup_timeout_ms),
            )
            .await?;
        spawn_drain(Arc::clone(&slot), notices);
        *slot.live.lock().await = Some(Connection {
            client: Arc::clone(&client),
            child: None,
            open_docs: HashMap::new(),
            started_at: Instant::now(),
        });
        *slot.state.write().await = LspServerState::Running;
        self.spawn_watcher(&slot, Arc::clone(&client));
        Ok(client)
    }
}

mod control;
mod documents;
mod status;
mod watch;

use self::documents::diagnostic_key;

#[cfg(test)]
pub(crate) mod tests;
