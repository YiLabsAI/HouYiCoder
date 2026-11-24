//! The connection manager: one supervised task per configured server, a
//! shared view of every server's state for the views, and the policy that
//! turns a dropped connection into a bounded restart.
//!
//! A server's slots live in one map ordered by name, so the tool list and the
//! instruction blocks are stable regardless of which handshake finished
//! first. A change notification does not trigger a refetch immediately: the
//! first notice opens a short window and everything arriving inside it folds
//! into one refetch, which is what keeps a server that re-lists in a loop
//! from spending the host's turn on it.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use houyicoder_api::launcher::ProcessLauncher;
use houyicoder_async::CancellationToken;
use houyicoder_protocol::mcp::{McpConnectionState, McpServerStatusView, ScopedMcpServerConfig};
use rmcp::RoleClient;
use rmcp::model::Tool;
use rmcp::service::Peer;
use tokio::sync::RwLock;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::sleep_until;
use tokio::time::timeout;

use super::client::{ClientEvent, LaunchedMcpServer, launch_stdio};
use super::liveness::RestartPolicy;
use super::stdio::{STDERR_TAIL_LINES, StderrTail};

/// The windows and limits one manager runs with. Production uses the
/// defaults; a test injects millisecond values so no assertion waits on a
/// wall clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpTimings {
    /// How long the first change notice waits for its siblings before the
    /// refetch runs.
    pub notice_window: Duration,
    /// How long a launch may spend in the handshake before it counts as
    /// failed.
    pub handshake_timeout: Duration,
    /// How often a connected server's transport is checked for closure.
    pub liveness_poll: Duration,
    /// How long a connection must stay up before its restart count resets. A
    /// connection that drops sooner is part of the same failure storm.
    pub stable_after: Duration,
    /// How long a closing handshake may take before the connection is
    /// dropped without one.
    pub close_timeout: Duration,
    /// How a dropped connection is retried.
    pub restart: RestartPolicy,
}

impl Default for McpTimings {
    fn default() -> Self {
        Self {
            notice_window: Duration::from_millis(50),
            handshake_timeout: Duration::from_secs(15),
            liveness_poll: Duration::from_millis(500),
            stable_after: Duration::from_secs(30),
            close_timeout: Duration::from_secs(2),
            restart: RestartPolicy::default(),
        }
    }
}

/// One server's shared view, what the views render and what the tool
/// adapter calls through.
pub(crate) struct ServerSlot {
    pub(crate) config: ScopedMcpServerConfig,
    pub(crate) state: McpConnectionState,
    pub(crate) tools: Vec<Tool>,
    pub(crate) instructions: Option<String>,
    pub(crate) server_version: Option<String>,
    pub(crate) stderr: StderrTail,
    pub(crate) peer: Option<Peer<RoleClient>>,
    wake: Option<mpsc::Sender<()>>,
}

/// The manager itself. Cloning shares the slots and the cancellation token,
/// so the supervisor tasks and the handles a view holds see one state.
#[derive(Clone)]
pub struct McpConnections {
    launcher: Arc<dyn ProcessLauncher>,
    workspace_root: Arc<Path>,
    timings: McpTimings,
    slots: Arc<RwLock<BTreeMap<String, ServerSlot>>>,
    cancel: CancellationToken,
    tasks: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// Bumped whenever the connected tool set changes, so the tool view can
    /// rebuild the moment a server connects, reconnects, or goes away
    /// instead of polling for a difference.
    generation: Arc<watch::Sender<u64>>,
}

impl McpConnections {
    /// A manager over one launcher and one workspace, with the given windows.
    pub fn new(
        launcher: Arc<dyn ProcessLauncher>,
        workspace_root: Arc<Path>,
        timings: McpTimings,
    ) -> Self {
        let (generation, _) = watch::channel(0);
        Self {
            launcher,
            workspace_root,
            timings,
            slots: Arc::new(RwLock::new(BTreeMap::new())),
            cancel: CancellationToken::new(),
            tasks: Arc::new(Mutex::new(Vec::new())),
            generation: Arc::new(generation),
        }
    }

    /// A receiver that fires once per change to the connected tool set.
    pub(crate) fn change_receiver(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    /// Announce that the connected tool set changed. Sending is best effort:
    /// no receiver means no tool source is subscribed, not an error.
    fn announce_change(&self) {
        self.generation.send_modify(|generation| *generation += 1);
    }

    /// Start every configured server as its own supervised task. A disabled
    /// server is recorded as disabled and never launched: the trust gate
    /// marks an untrusted workspace's servers this way, and no process or
    /// request may follow from one.
    pub async fn start_all(&self, servers: Vec<ScopedMcpServerConfig>) {
        for config in servers {
            let name = config.name.clone();
            if config.disabled {
                let mut slots = self.slots.write().await;
                slots.insert(
                    name,
                    ServerSlot {
                        config,
                        state: McpConnectionState::Disabled {
                            reason: "disabled by configuration or workspace trust".into(),
                        },
                        tools: Vec::new(),
                        instructions: None,
                        server_version: None,
                        stderr: StderrTail::default(),
                        peer: None,
                        wake: None,
                    },
                );
                continue;
            }
            let (wake_tx, wake_rx) = mpsc::channel(4);
            {
                let mut slots = self.slots.write().await;
                slots.insert(
                    name.clone(),
                    ServerSlot {
                        config,
                        state: McpConnectionState::Pending,
                        tools: Vec::new(),
                        instructions: None,
                        server_version: None,
                        stderr: StderrTail::default(),
                        peer: None,
                        wake: Some(wake_tx),
                    },
                );
            }
            let manager = self.clone();
            let task = tokio::spawn(async move {
                manager.supervise(name, wake_rx).await;
            });
            if let Ok(mut tasks) = self.tasks.lock() {
                tasks.push(task);
            }
        }
    }

    /// The servers as the status views render them, ordered by name.
    pub async fn statuses(&self) -> Vec<McpServerStatusView> {
        let slots = self.slots.read().await;
        slots
            .values()
            .map(|slot| McpServerStatusView {
                name: slot.config.name.clone(),
                scope: slot.config.scope,
                state: slot.state.clone(),
                server_version: slot.server_version.clone(),
                tool_count: slot.tools.len(),
                resource_count: 0,
                has_instructions: slot
                    .instructions
                    .as_deref()
                    .is_some_and(|text| !text.is_empty()),
                recent_stderr: slot.stderr.lines(),
            })
            .collect()
    }

    /// The most recent diagnostic lines one server wrote, oldest first.
    pub async fn stderr_tail(&self, name: &str) -> Vec<String> {
        let slots = self.slots.read().await;
        slots
            .get(name)
            .map(|slot| slot.stderr.lines())
            .unwrap_or_default()
    }

    /// The diagnostics hold size the tail keeps, exposed so a status view
    /// can say how much of the child's output it is eliding.
    pub fn stderr_limit(&self) -> usize {
        STDERR_TAIL_LINES
    }

    /// Read the instructions exported by connected servers, sorted by server
    /// name, as one block of explicit server tags for the prompt. Server text
    /// is untrusted, so each entry is wrapped in a tag naming its server, and
    /// both the text and the name are escaped: server content cannot close the
    /// host's frame or leave the attribute. A server the allowed list excludes
    /// contributes neither its instructions nor a tag. Each server's text is
    /// cut at a character boundary to 4,096 characters.
    pub async fn instructions_for(&self, allowed_servers: Option<&[String]>) -> Option<String> {
        const MAX_MCP_INSTRUCTIONS_CHARS: usize = 4_096;
        let slots = self.slots.read().await;
        let mut sections = Vec::new();
        for (name, slot) in slots.iter() {
            if !matches!(slot.state, McpConnectionState::Connected) {
                continue;
            }
            if let Some(allowed) = allowed_servers
                && !allowed.iter().any(|a| a == name)
            {
                continue;
            }
            if let Some(instructions) = slot
                .instructions
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                let trimmed = instructions.trim();
                let bounded: String = if trimmed.chars().count() > MAX_MCP_INSTRUCTIONS_CHARS {
                    trimmed.chars().take(MAX_MCP_INSTRUCTIONS_CHARS).collect()
                } else {
                    trimmed.to_string()
                };
                sections.push(format!(
                    "<mcp_server_instructions server=\"{}\">\n{}\n</mcp_server_instructions>",
                    escape_attribute(name),
                    escape_text(&bounded)
                ));
            }
        }
        if sections.is_empty() {
            None
        } else {
            Some(sections.join("\n\n"))
        }
    }

    /// Ask one server to reconnect now, whether it failed or the user wants a
    /// fresh handshake.
    pub async fn reconnect(&self, name: &str) {
        let slots = self.slots.read().await;
        if let Some(wake) = slots.get(name).and_then(|slot| slot.wake.clone()) {
            let _ = wake.try_send(());
        }
    }

    /// Stop every server: cancel their services, wait for their tasks, and
    /// let the launcher's on-drop kill end each process group.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        let tasks: Vec<JoinHandle<()>> = match self.tasks.lock() {
            Ok(mut tasks) => tasks.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        for task in tasks {
            drop(task.await);
        }
    }

    /// The supervisor for one server.
    async fn supervise(&self, name: String, mut wake: mpsc::Receiver<()>) {
        let mut attempt = 0u32;
        loop {
            let Some(config) = self.slot_config(&name).await else {
                return;
            };
            self.set_state(&name, McpConnectionState::Pending).await;
            let mut live = match self.connect(&config).await {
                Ok(live) => live,
                Err(error) => {
                    self.set_state(&name, McpConnectionState::Failed { error })
                        .await;
                    if !self.wait_wake(&mut wake).await {
                        return;
                    }
                    attempt = 0;
                    continue;
                }
            };
            self.mark_connected(&name, &mut live).await;
            let connected_at = Instant::now();
            let end = self.serve_until_end(&name, &mut live, &mut wake).await;
            // A connection that stayed up past the stability window resets
            // the count; one that dropped sooner joins the same storm.
            if connected_at.elapsed() >= self.timings.stable_after {
                attempt = 0;
            }
            drop(
                live.service
                    .close_with_timeout(self.timings.close_timeout)
                    .await,
            );
            match end {
                ServeEnd::Shutdown => return,
                ServeEnd::Wake => {
                    // The user asked for a fresh handshake: the old peer and
                    // its tools go now, so a relaunch that fails cannot leave
                    // the source advertising a server nothing is behind.
                    self.forget_connection(&name).await;
                }
                ServeEnd::Closed => {
                    // The peer is dead: drop it and the tools it served, so
                    // nothing keeps calling a transport that cannot answer.
                    self.forget_connection(&name).await;
                    attempt += 1;
                    match self.timings.restart.delay(attempt) {
                        Some(delay) => {
                            self.set_state(&name, McpConnectionState::Reconnecting { attempt })
                                .await;
                            sleep(delay).await;
                        }
                        None => {
                            self.set_state(
                                &name,
                                McpConnectionState::Failed {
                                    error: "the connection kept dropping".into(),
                                },
                            )
                            .await;
                            if !self.wait_wake(&mut wake).await {
                                return;
                            }
                            attempt = 0;
                        }
                    }
                }
            }
        }
    }

    /// Launch one server and wait out its handshake inside the window, or
    /// name why it did not arrive. A shutdown ends the wait immediately, so a
    /// session close never has to outlast a slow handshake.
    async fn connect(&self, config: &ScopedMcpServerConfig) -> Result<LaunchedMcpServer, String> {
        let launched = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err("the manager is shutting down".into()),
            launched = timeout(
                self.timings.handshake_timeout,
                launch_stdio(
                    &self.launcher,
                    &config.transport,
                    self.workspace_root.clone(),
                ),
            ) => launched,
        };
        match launched {
            Ok(Ok(live)) => Ok(live),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_elapsed) => Err("the handshake timed out".into()),
        }
    }

    /// Fetch the server's identity, instructions, and tools, and publish them
    /// to the shared view.
    async fn mark_connected(&self, name: &str, live: &mut LaunchedMcpServer) {
        let info = live.service.peer_info();
        let tools =
            match timeout(self.timings.handshake_timeout, live.peer().list_tools(None)).await {
                Ok(Ok(result)) => result.tools,
                _ => Vec::new(),
            };
        let mut slots = self.slots.write().await;
        if let Some(slot) = slots.get_mut(name) {
            slot.state = McpConnectionState::Connected;
            slot.peer = Some(live.peer().clone());
            slot.tools = tools;
            slot.instructions = info.as_ref().and_then(|i| i.instructions.clone());
            slot.server_version = info
                .as_ref()
                .and_then(|i| i.server_info.as_ref().map(|s| s.version.clone()));
        }
        drop(slots);
        // A reconnect hands out a new peer, so the tools must be rebuilt
        // even when the names are unchanged.
        self.announce_change();
    }

    /// Run the connection until it ends, folding change notices into one
    /// refetch per window.
    async fn serve_until_end(
        &self,
        name: &str,
        live: &mut LaunchedMcpServer,
        wake: &mut mpsc::Receiver<()>,
    ) -> ServeEnd {
        let mut pending: BTreeSet<NoticeKind> = BTreeSet::new();
        let mut window = None;
        loop {
            let deadline = window.unwrap_or_else(|| Instant::now() + self.timings.notice_window);
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => return ServeEnd::Shutdown,
                _ = wake.recv() => return ServeEnd::Wake,
                _ = sleep_until_deadline(window, deadline), if window.is_some() => {
                    self.refetch(name, live, &pending).await;
                    pending.clear();
                    window = None;
                }
                _ = sleep(self.timings.liveness_poll) => {
                    if live.peer().is_transport_closed() {
                        return ServeEnd::Closed;
                    }
                }
                event = live.events.recv() => match event {
                    Some(kind) => {
                        pending.insert(NoticeKind::from(kind));
                        if window.is_none() {
                            window = Some(Instant::now() + self.timings.notice_window);
                        }
                    }
                    None => return ServeEnd::Closed,
                },
            }
        }
    }

    /// Refetch what the received notices named, and publish the result.
    async fn refetch(
        &self,
        name: &str,
        live: &mut LaunchedMcpServer,
        pending: &BTreeSet<NoticeKind>,
    ) {
        if pending.contains(&NoticeKind::Tools)
            && let Ok(Ok(result)) =
                timeout(self.timings.handshake_timeout, live.peer().list_tools(None)).await
        {
            let mut slots = self.slots.write().await;
            if let Some(slot) = slots.get_mut(name) {
                slot.tools = result.tools;
            }
            drop(slots);
            self.announce_change();
        }
    }

    /// Drop a server's peer and its tools after the connection ended, and
    /// announce the change so nothing keeps addressing a dead transport.
    async fn forget_connection(&self, name: &str) {
        let mut slots = self.slots.write().await;
        if let Some(slot) = slots.get_mut(name) {
            slot.peer = None;
            slot.tools.clear();
            slot.instructions = None;
        }
        drop(slots);
        self.announce_change();
    }

    async fn wait_wake(&self, wake: &mut mpsc::Receiver<()>) -> bool {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => false,
            result = wake.recv() => result.is_some(),
        }
    }

    async fn slot_config(&self, name: &str) -> Option<ScopedMcpServerConfig> {
        self.slots
            .read()
            .await
            .get(name)
            .map(|slot| slot.config.clone())
    }

    async fn set_state(&self, name: &str, state: McpConnectionState) {
        let mut slots = self.slots.write().await;
        if let Some(slot) = slots.get_mut(name) {
            slot.state = state;
        }
    }

    /// How long one call on this server may run.
    pub(crate) async fn call_timeout_of(&self, name: &str) -> Duration {
        self.slots
            .read()
            .await
            .get(name)
            .map(|slot| Duration::from_millis(slot.config.tool_timeout_ms))
            .unwrap_or(Duration::from_secs(60))
    }

    /// A token that fires when the manager shuts down. It is a child of the
    /// manager's own, so a watcher that cancels it ends its own wait without
    /// stopping the servers.
    pub(crate) fn watch_stop(&self) -> CancellationToken {
        self.cancel.child_token()
    }

    /// One server's peer, for the tool adapter to call through.
    pub(crate) async fn peer_of(&self, name: &str) -> Option<Peer<RoleClient>> {
        self.slots
            .read()
            .await
            .get(name)
            .and_then(|slot| slot.peer.clone())
    }

    /// The tools every connected server reports, ordered by server name then
    /// tool name, so the exposed list is stable across handshake order.
    pub(crate) async fn tools_snapshot(&self) -> Vec<(String, Tool)> {
        let slots = self.slots.read().await;
        let mut all = Vec::new();
        for slot in slots.values() {
            if !matches!(slot.state, McpConnectionState::Connected) {
                continue;
            }
            let mut tools = slot.tools.clone();
            tools.sort_by(|a, b| a.name.cmp(&b.name));
            for tool in tools {
                all.push((slot.config.name.clone(), tool));
            }
        }
        all
    }
}

/// Why the serve loop ended.
enum ServeEnd {
    /// The manager is shutting down.
    Shutdown,
    /// The user asked for a reconnect.
    Wake,
    /// The transport closed.
    Closed,
}

/// What one notice asked to refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NoticeKind {
    Tools,
    Resources,
    Prompts,
}

impl From<ClientEvent> for NoticeKind {
    fn from(event: ClientEvent) -> Self {
        match event {
            ClientEvent::ToolListChanged => Self::Tools,
            ClientEvent::ResourceListChanged => Self::Resources,
            ClientEvent::PromptListChanged => Self::Prompts,
        }
    }
}

/// Escape the characters that would let server text close the host's tag.
fn escape_text(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape a server name for use inside a double-quoted attribute.
fn escape_attribute(name: &str) -> String {
    escape_text(name).replace('"', "&quot;")
}

/// Wait for a window deadline, or forever when no window is open. A pending
/// sleep in a select arm needs a future even when the guard is false, so the
/// closed case parks instead of spinning.
async fn sleep_until_deadline(window: Option<Instant>, deadline: Instant) {
    match window {
        Some(_) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
pub(crate) mod tests;
