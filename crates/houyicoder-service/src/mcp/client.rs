//! The client side of one external tool server: the handler that answers the
//! server's reverse requests and reports its change notifications, and the
//! launch that starts a stdio server through the process launcher and hands
//! its pipes to the protocol transport.

// The protocol deprecates roots in favor of a replacement tool servers do not
// ask for yet; this module answers the handshake they do send.
#![allow(deprecated)]

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::fmt::{Display, Formatter};
use std::future::ready;
use std::path::Path;
use std::sync::Arc;

use houyicoder_api::launcher::{
    LauncherChild, ProcessLauncher, SpawnPolicy, SpawnRequest, StdioConfig, StdioMode, StdioPipes,
};
use houyicoder_protocol::mcp::McpTransportConfig;
use rmcp::ClientHandler;
use rmcp::RoleClient;
use rmcp::ServiceExt;
use rmcp::model::{
    ClientCapabilities, ErrorData, Implementation, InitializeRequestParams, ListRootsResult, Root,
    RootsCapabilities,
};
use rmcp::service::{NotificationContext, Peer, RequestContext, RunningService};
use rmcp::transport::async_rw::AsyncRwTransport;
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;

use super::stdio::{Pipes, StderrTail, start};

/// Why a server could not be launched or handed to the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpClientError {
    /// The launcher refused the spawn or gave back no pipes.
    Launch(String),
    /// The transport could not be handed to the protocol client.
    Serve(String),
    /// The server asked for credentials before it would talk.
    NeedsAuth(String),
}

impl Display for McpClientError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Launch(reason) => write!(f, "the server did not start: {reason}"),
            Self::Serve(reason) => write!(f, "the protocol client did not start: {reason}"),
            Self::NeedsAuth(reason) => write!(f, "the server requires credentials: {reason}"),
        }
    }
}

impl Error for McpClientError {}

/// What a server told the client changed, in the words the manager needs.
/// One variant per notification kind, so a coalescer can key on the pair of
/// server and kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::enum_variant_names,
    reason = "each variant names the notification it carries"
)]
pub enum ClientEvent {
    ToolListChanged,
    ResourceListChanged,
    PromptListChanged,
}

/// Answers a server's reverse requests and reports its notifications.
///
/// A tool server may ask which directories this session works in; the answer
/// is the workspace the session was opened on, as a standard file URI. It may
/// also ask the client to run an inference; that request is refused, because
/// a server must not spend the user's model budget.
pub struct McpClientHandler {
    workspace_root: Arc<Path>,
    events: Option<UnboundedSender<ClientEvent>>,
}

impl McpClientHandler {
    /// A handler that answers for this workspace and reports changes on the
    /// given channel.
    pub fn new(workspace_root: Arc<Path>, events: UnboundedSender<ClientEvent>) -> Self {
        Self {
            workspace_root,
            events: Some(events),
        }
    }

    /// The client identity and capabilities sent in the handshake.
    fn handshake_info(&self) -> InitializeRequestParams {
        // Roots are how a server learns the session's directories, and the
        // change notification tells it when they move. The capability is
        // deprecated in the protocol but still what servers ask for, so it
        // stays on until the replacement is as widely read.
        let mut roots = RootsCapabilities::default();
        roots.list_changed = Some(true);
        let mut capabilities = ClientCapabilities::default();
        capabilities.roots = Some(roots);
        InitializeRequestParams::new(
            capabilities,
            Implementation::new("houyicoder", env!("CARGO_PKG_VERSION")),
        )
    }

    /// The roots this session declares: the workspace it was opened on.
    pub fn roots(&self) -> ListRootsResult {
        let root = Root::new(file_uri_for(self.workspace_root.as_ref())).with_name("workspace");
        ListRootsResult::new(vec![root])
    }

    fn report(&self, event: ClientEvent) {
        if let Some(events) = &self.events
            && events.send(event).is_err()
        {
            // The manager dropped its receiver: the server is being torn down
            // and a lost change notice costs a refetch nobody is waiting for.
            tracing::debug!("the change channel is closed");
        }
    }
}

impl ClientHandler for McpClientHandler {
    fn get_info(&self) -> InitializeRequestParams {
        self.handshake_info()
    }

    fn list_roots(
        &self,
        _context: RequestContext<RoleClient>,
    ) -> impl Future<Output = Result<ListRootsResult, ErrorData>> + Send + '_ {
        ready(Ok(self.roots()))
    }

    fn on_tool_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.report(ClientEvent::ToolListChanged);
        ready(())
    }

    fn on_resource_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.report(ClientEvent::ResourceListChanged);
        ready(())
    }

    fn on_prompt_list_changed(
        &self,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.report(ClientEvent::PromptListChanged);
        ready(())
    }
}

/// One live server: the protocol service, the child whose lifetime it runs
/// on when it has one, and the tail of the diagnostics it wrote.
pub struct LaunchedMcpServer {
    pub service: RunningService<RoleClient, McpClientHandler>,
    /// Dropped with a stdio server; the launcher's on-drop kill then ends the
    /// child and its process group. A remote server has no child.
    pub child: Option<LauncherChild>,
    pub stderr: StderrTail,
    pub events: UnboundedReceiver<ClientEvent>,
}

impl LaunchedMcpServer {
    /// The protocol peer, for tool calls and listings.
    pub fn peer(&self) -> &Peer<RoleClient> {
        self.service.peer()
    }
}

/// Start one stdio server: launch the child through the launcher, bridge its
/// pipes, and hand the transport to the protocol client.
pub async fn launch_stdio(
    launcher: &Arc<dyn ProcessLauncher>,
    transport: &McpTransportConfig,
    workspace_root: Arc<Path>,
) -> Result<LaunchedMcpServer, McpClientError> {
    let request = stdio_spawn_request(transport, workspace_root.as_ref(), &host_environment());
    let mut child = launcher
        .spawn(request, SpawnPolicy::default().sandboxed())
        .map_err(|error| McpClientError::Launch(error.to_string()))?;
    let pipes: StdioPipes = child
        .take_pipes()
        .ok_or_else(|| McpClientError::Launch("the launcher returned no pipes".into()))?;
    let (stdin, stdout, stderr) = match (pipes.stdin, pipes.stdout) {
        (Some(stdin), Some(stdout)) => (stdin, stdout, pipes.stderr),
        _ => {
            return Err(McpClientError::Launch(
                "the launcher returned an incomplete pipe pair".into(),
            ));
        }
    };
    let (bridge, stderr_tail) = start(Pipes {
        stdin,
        stdout,
        stderr,
    });
    let (service, events) = serve_over(bridge.reader, bridge.writer, workspace_root).await?;
    Ok(LaunchedMcpServer {
        service,
        child: Some(child),
        stderr: stderr_tail,
        events,
    })
}

/// Run the protocol handshake and the service loop over one pair of byte
/// streams. Split from the launch so a test can hand it an in-memory peer.
pub(crate) async fn serve_over(
    reader: DuplexStream,
    writer: DuplexStream,
    workspace_root: Arc<Path>,
) -> Result<
    (
        RunningService<RoleClient, McpClientHandler>,
        UnboundedReceiver<ClientEvent>,
    ),
    McpClientError,
> {
    let (events_tx, events) = mpsc::unbounded_channel();
    let handler = McpClientHandler::new(workspace_root, events_tx);
    let service = handler
        .serve(AsyncRwTransport::new_client(reader, writer))
        .await
        .map_err(|error| McpClientError::Serve(error.to_string()))?;
    Ok((service, events))
}

/// The spawn request one stdio server runs under: the launcher's policy, the
/// workspace as its directory, and an environment of the base runtime set
/// plus exactly what the configuration names.
pub fn stdio_spawn_request(
    transport: &McpTransportConfig,
    workspace_root: &Path,
    host: &[(String, String)],
) -> SpawnRequest {
    let mut request = SpawnRequest::new(transport.endpoint.clone())
        .with_args(transport.arguments.clone())
        .with_workspace(workspace_root.to_path_buf())
        .with_env(child_environment(&transport.environment, host))
        .interactive();
    request.stdio = StdioConfig::piped_output();
    request.stdio.stdin = StdioMode::Piped;
    request
}

/// Variables a child process needs to function regardless of configuration.
const BASE_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "SYSTEMROOT",
    "COMSPEC",
];

/// The environment one child receives: the base runtime set from the host,
/// plus the variables the configuration sets, which win over the host's.
pub fn child_environment(
    configured: &BTreeMap<String, String>,
    host: &[(String, String)],
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = host
        .iter()
        .filter(|(key, value)| {
            BASE_VARS.contains(&key.as_str()) && !value.is_empty() && !configured.contains_key(key)
        })
        .cloned()
        .collect();
    for (key, value) in configured {
        env.retain(|(existing, _)| existing != key);
        env.push((key.clone(), value.clone()));
    }
    env
}

/// The host environment, read once per launch.
fn host_environment() -> Vec<(String, String)> {
    env::vars().collect()
}

/// The standard file URI for one directory, with the drive spelling a
/// Windows client expects.
pub fn file_uri_for(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let encoded: String = text
        .chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '#' => "%23".to_string(),
            '?' => "%3F".to_string(),
            other => other.to_string(),
        })
        .collect();
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        // A Windows drive path has no leading slash; the URI adds one after
        // the authority, so the result reads file:///C:/...
        format!("file:///{encoded}")
    }
}

// Visible to the sibling test modules that build a peer to call through,
// which is what keeps one in-memory peer double in the tree.
#[cfg(test)]
pub(crate) mod tests;
