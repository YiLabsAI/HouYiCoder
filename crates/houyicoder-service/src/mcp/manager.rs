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
use std::iter::once;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use houyicoder_api::launcher::ProcessLauncher;
use houyicoder_async::CancellationToken;
use houyicoder_protocol::mcp::{McpConnectionState, McpTransportKind, ScopedMcpServerConfig};
use rmcp::RoleClient;
use rmcp::model::ReadResourceRequestParams;
use rmcp::model::Resource;
use rmcp::model::ResourceContents;
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

use super::client::{ClientEvent, LaunchedMcpServer, McpClientError, launch_stdio};
use super::http;
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
    pub(crate) resources: Vec<Resource>,
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
                        resources: Vec::new(),
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
                        resources: Vec::new(),
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
                    // A server that asked for credentials cannot be helped by
                    // retrying, so the state says so and the supervisor waits
                    // for a user reconnect either way.
                    self.set_state(&name, state_after_launch_error(error)).await;
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
    async fn connect(
        &self,
        config: &ScopedMcpServerConfig,
    ) -> Result<LaunchedMcpServer, McpClientError> {
        let launch = async {
            match config.transport.kind {
                McpTransportKind::Stdio => {
                    launch_stdio(
                        &self.launcher,
                        &config.transport,
                        self.workspace_root.clone(),
                    )
                    .await
                }
                McpTransportKind::Http => {
                    http::launch_http(
                        &self.launcher,
                        &config.transport,
                        self.workspace_root.clone(),
                    )
                    .await
                }
            }
        };
        let launched = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                return Err(McpClientError::Serve("the manager is shutting down".into()));
            }
            launched = timeout(self.timings.handshake_timeout, launch) => launched,
        };
        match launched {
            Ok(result) => result,
            Err(_elapsed) => Err(McpClientError::Serve("the handshake timed out".into())),
        }
    }

    /// Fetch the server's identity, instructions, tools, and resources, and
    /// publish them to the shared view.
    async fn mark_connected(&self, name: &str, live: &mut LaunchedMcpServer) {
        let info = live.service.peer_info();
        let peer = live.peer().clone();
        let budget = self.timings.handshake_timeout;
        let (tools, resources) = tokio::join!(
            timeout(budget, peer.list_tools(None)),
            resources_of(&peer, budget),
        );
        let tools = match tools {
            Ok(Ok(result)) => result.tools,
            _ => Vec::new(),
        };
        let mut slots = self.slots.write().await;
        if let Some(slot) = slots.get_mut(name) {
            slot.state = McpConnectionState::Connected;
            slot.peer = Some(peer);
            slot.tools = tools;
            // A fresh connection is a new server identity, so an unanswered
            // resource listing starts empty rather than carrying the previous
            // connection's list.
            slot.resources = resources.unwrap_or_default();
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
        let want_tools = pending.contains(&NoticeKind::Tools);
        let want_resources = pending.contains(&NoticeKind::Resources);
        if !want_tools && !want_resources {
            return;
        }
        let peer = live.peer().clone();
        let budget = self.timings.handshake_timeout;
        let refresh = async {
            let tools = async {
                if !want_tools {
                    return None;
                }
                match timeout(budget, peer.list_tools(None)).await {
                    Ok(Ok(result)) => Some(result.tools),
                    _ => None,
                }
            };
            let resources = async {
                if !want_resources {
                    return None;
                }
                resources_of(&peer, budget).await
            };
            tokio::join!(tools, resources)
        };
        // A listing that does not answer is not a listing of nothing, so only
        // an answer replaces what the slot already knows.
        let (tools, resources) = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return,
            listed = refresh => listed,
        };
        let mut changed = false;
        let mut slots = self.slots.write().await;
        if let Some(slot) = slots.get_mut(name) {
            if let Some(tools) = tools {
                slot.tools = tools;
                changed = true;
            }
            if let Some(resources) = resources {
                slot.resources = resources;
                changed = true;
            }
        }
        drop(slots);
        if changed {
            self.announce_change();
        }
    }

    /// Drop a server's peer and what it served after the connection ended,
    /// and announce the change so nothing keeps addressing a dead transport.
    async fn forget_connection(&self, name: &str) {
        let mut slots = self.slots.write().await;
        if let Some(slot) = slots.get_mut(name) {
            slot.peer = None;
            slot.tools.clear();
            slot.resources.clear();
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

    /// The resources every connected server lists, ordered by server name
    /// then resource name. One server may be named to narrow the view; a name
    /// the manager does not hold, or holds without a connection, is an error
    /// rather than an empty listing.
    pub(crate) async fn resources_for(
        &self,
        server: Option<&str>,
    ) -> Result<Vec<(String, Resource)>, String> {
        let slots = self.slots.read().await;
        if let Some(only) = server {
            let slot = slots
                .get(only)
                .ok_or_else(|| format!("no server named {only} is configured"))?;
            if !matches!(slot.state, McpConnectionState::Connected) {
                return Err(format!("{only} is not connected"));
            }
            return Ok(sorted_resources(once(slot)));
        }
        Ok(sorted_resources(slots.values().filter(|slot| {
            matches!(slot.state, McpConnectionState::Connected)
        })))
    }

    /// Read one resource from one connected server, inside that server's call
    /// budget.
    pub(crate) async fn read_resource(
        &self,
        server: &str,
        uri: &str,
    ) -> Result<Vec<ResourceContents>, String> {
        let (peer, budget) = {
            let slots = self.slots.read().await;
            let slot = slots
                .get(server)
                .ok_or_else(|| format!("no server named {server} is configured"))?;
            if !matches!(slot.state, McpConnectionState::Connected) {
                return Err(format!("{server} is not connected"));
            }
            (
                slot.peer.clone(),
                Duration::from_millis(slot.config.tool_timeout_ms),
            )
        };
        let peer = peer.ok_or_else(|| format!("{server} has no live connection"))?;
        let answer = timeout(
            budget,
            peer.read_resource(ReadResourceRequestParams::new(uri)),
        )
        .await
        .map_err(|_| format!("{server} did not answer inside its budget"))?;
        let result = answer.map_err(|error| format!("{server} refused the read: {error}"))?;
        Ok(result.contents)
    }
}

/// Every resource of the given slots, ordered by server name then resource
/// name.
fn sorted_resources<'a>(slots: impl Iterator<Item = &'a ServerSlot>) -> Vec<(String, Resource)> {
    let mut all = Vec::new();
    for slot in slots {
        let mut resources = slot.resources.clone();
        resources.sort_by(|a, b| a.name.cmp(&b.name));
        for resource in resources {
            all.push((slot.config.name.clone(), resource));
        }
    }
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all
}

/// The resources one server lists, bounded by the handshake window. None
/// means the server did not answer, which is not the same as an empty list:
/// a refresh that gets None keeps what the slot already knows.
async fn resources_of(peer: &Peer<RoleClient>, budget: Duration) -> Option<Vec<Resource>> {
    match timeout(budget, peer.list_resources(None)).await {
        Ok(Ok(result)) => Some(result.resources),
        Ok(Err(error)) => {
            tracing::warn!(%error, "a server's resource listing failed; the known list is kept");
            None
        }
        Err(_) => {
            tracing::warn!("a server's resource listing timed out; the known list is kept");
            None
        }
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

/// The state a server's view records after a launch that did not connect.
fn state_after_launch_error(error: McpClientError) -> McpConnectionState {
    match error {
        McpClientError::NeedsAuth(reason) => McpConnectionState::NeedsAuth { reason },
        other => McpConnectionState::Failed {
            error: other.to_string(),
        },
    }
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

mod views;
pub(crate) use views::instructions_block;

#[cfg(test)]
pub(crate) mod tests;
