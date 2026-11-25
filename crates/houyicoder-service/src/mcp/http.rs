//! The streamable HTTP transport for remote tool servers: the client built
//! from a resolved entry, the helper that supplies request headers, and the
//! reconnect delays the transport applies.

mod throttle;

use throttle::ThrottledHttpClient;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::executor::block_on;
use houyicoder_api::launcher::FenceConfig;
use houyicoder_api::launcher::ProcessLauncher;
use houyicoder_api::launcher::SpawnPolicy;
use houyicoder_api::launcher::SpawnRequest;
use houyicoder_platform::host::default_helper_program;
use houyicoder_platform::host::net::ProxyConfig;
use houyicoder_platform::host::net::ProxyConfigCache;
use houyicoder_platform::tls::install_tls_provider;
use houyicoder_protocol::mcp::McpTransportConfig;
use reqwest::Client;
use reqwest::NoProxy;
use reqwest::Proxy;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use rmcp::ServiceExt;
use rmcp::service::ClientInitializeError;
use rmcp::transport::common::client_side_sse::ExponentialBackoff;
use rmcp::transport::streamable_http_client::StreamableHttpClient;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use tokio::sync::mpsc::unbounded_channel;
use tokio::task::spawn_blocking;
use tokio::time::timeout;

use super::client::LaunchedMcpServer;
use super::client::McpClientError;
use super::client::McpClientHandler;
use super::stdio::StderrTail;

/// How long a header helper may run before the server starts without its
/// headers.
const HEADER_HELPER_TIMEOUT: Duration = Duration::from_secs(5);

/// The first reconnect delay after the transport fails to redial.
pub const BACKOFF_BASE: Duration = Duration::from_millis(500);

/// The longest delay between reconnect attempts.
pub const BACKOFF_CAP: Duration = Duration::from_secs(30);

/// The header names the transport owns. A configured header with one of these
/// names is skipped with the name reported, so a stale value cannot ride
/// ahead of the one the transport sets per request.
const RESERVED_HEADERS: &[&str] = &[
    "accept",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// Start one remote server: build its client from the config, hand the
/// streamable transport to the protocol client, and keep the service.
pub(crate) async fn launch_http(
    launcher: &Arc<dyn ProcessLauncher>,
    transport: &McpTransportConfig,
    workspace_root: Arc<Path>,
) -> Result<LaunchedMcpServer, McpClientError> {
    let (client, config, helper_note) =
        http_client_for(launcher, transport, workspace_root.as_ref()).await?;
    serve_with(client, config, workspace_root)
        .await
        .map_err(|error| with_helper_note(error, helper_note))
}

/// The client and transport config one remote server starts with: the proxy
/// policy the rest of the application uses, headers resolved, and the
/// reconnect delays attached.
async fn http_client_for(
    launcher: &Arc<dyn ProcessLauncher>,
    transport: &McpTransportConfig,
    workspace: &Path,
) -> Result<
    (
        ThrottledHttpClient<Client>,
        StreamableHttpClientTransportConfig,
        Option<String>,
    ),
    McpClientError,
> {
    install_tls_provider();
    let (headers, helper_note) = request_headers(launcher, transport, workspace).await;
    let client = ThrottledHttpClient::new(proxy_client().map_err(McpClientError::Launch)?);
    let mut config = StreamableHttpClientTransportConfig::with_uri(transport.endpoint.clone());
    config.custom_headers = custom_headers(&headers);
    let mut backoff = ExponentialBackoff::default();
    backoff.base_duration = BACKOFF_BASE;
    backoff.max_delay = Some(BACKOFF_CAP);
    config.retry_config = Arc::new(backoff);
    Ok((client, config, helper_note))
}

/// The client for the current proxy snapshot.
fn proxy_client() -> Result<Client, String> {
    let snapshot = ProxyConfigCache::global().snapshot();
    build_proxy_client(
        snapshot.proxy(),
        snapshot.diagnostic().map(ToString::to_string),
    )
}

/// The client for one resolved proxy policy: direct when none resolved, the
/// configured endpoints otherwise, and a configuration that could not be
/// evaluated fails instead of connecting direct.
fn build_proxy_client(
    proxy: Option<&ProxyConfig>,
    diagnostic: Option<String>,
) -> Result<Client, String> {
    if let Some(diagnostic) = diagnostic {
        return Err(format!("the proxy configuration is unusable: {diagnostic}"));
    }
    let mut builder = Client::builder();
    let Some(proxy) = proxy else {
        return builder
            .no_proxy()
            .build()
            .map_err(|error| format!("the http client could not be built: {error}"));
    };
    let bypass = (!proxy.exceptions.is_empty())
        .then(|| NoProxy::from_string(&proxy.exceptions))
        .flatten();
    for (is_https, endpoint) in [(true, &proxy.https), (false, &proxy.http)] {
        let Some(endpoint) = endpoint else {
            continue;
        };
        let entry = if is_https {
            Proxy::https(endpoint)
        } else {
            Proxy::http(endpoint)
        }
        .map_err(|error| format!("the proxy endpoint is not usable: {error}"))?;
        builder = builder.proxy(entry.no_proxy(bypass.clone()));
    }
    builder
        .build()
        .map_err(|error| format!("the http client could not be built: {error}"))
}

/// Hand one HTTP client to the protocol client and keep the service it
/// produces. Split from the client construction so a test can drive the
/// handshake over a client it controls, with no network involved.
async fn serve_with<C>(
    client: C,
    config: StreamableHttpClientTransportConfig,
    workspace_root: Arc<Path>,
) -> Result<LaunchedMcpServer, McpClientError>
where
    C: StreamableHttpClient,
{
    let (events_tx, events) = unbounded_channel();
    let handler = McpClientHandler::new(workspace_root, events_tx);
    let service = handler
        .serve(StreamableHttpClientTransport::with_client(client, config))
        .await
        .map_err(classify_launch_error)?;
    Ok(LaunchedMcpServer {
        service,
        child: None,
        stderr: StderrTail::default(),
        events,
    })
}

/// The service error an HTTP launch failed with, classified so the manager
/// can tell a server that wants credentials from one that broke. The
/// challenge's scheme reaches the user; its parameters stay out of the view.
fn classify_launch_error(error: ClientInitializeError) -> McpClientError {
    match error.auth_challenge() {
        Some(challenge) => {
            let scheme = challenge.split_whitespace().next().unwrap_or("credentials");
            McpClientError::NeedsAuth(format!("the server requires {scheme} credentials"))
        }
        None => McpClientError::Serve(error.to_string()),
    }
}

/// Attach a helper failure to a launch failure, so a server that rejected
/// unauthenticated requests names the helper the user configured.
fn with_helper_note(error: McpClientError, note: Option<String>) -> McpClientError {
    let Some(note) = note else {
        return error;
    };
    match error {
        McpClientError::NeedsAuth(reason) => McpClientError::NeedsAuth(format!("{reason}; {note}")),
        McpClientError::Serve(reason) => McpClientError::Serve(format!("{reason}; {note}")),
        other => other,
    }
}

/// The request headers one remote server sends: its configured headers, with
/// a helper's answer layered on top when the entry names one. The returned
/// note names a helper that did not answer, for the failure the launch may
/// report; header values never enter it.
async fn request_headers(
    launcher: &Arc<dyn ProcessLauncher>,
    transport: &McpTransportConfig,
    workspace: &Path,
) -> (BTreeMap<String, String>, Option<String>) {
    let mut headers = transport.headers.clone();
    let Some(helper) = transport.headers_helper.as_deref() else {
        return (headers, None);
    };
    match run_header_helper(launcher, helper, workspace).await {
        Ok(supplied) => {
            headers.extend(supplied);
            (headers, None)
        }
        Err(reason) => (
            headers,
            Some(format!("the header helper did not answer: {reason}")),
        ),
    }
}

/// Run the configured header helper through the launcher's fence and read one
/// JSON object of string values from its output. The capture path blocks, so
/// it runs on a blocking worker and the budget is enforced here as well as by
/// the fence. The helper shell is the POSIX shell on every host; a host
/// without one starts the server with the configured headers only.
async fn run_header_helper(
    launcher: &Arc<dyn ProcessLauncher>,
    command: &str,
    workspace: &Path,
) -> Result<BTreeMap<String, String>, &'static str> {
    let program = default_helper_program();
    let request = SpawnRequest::new(program.executable().to_string_lossy().into_owned())
        .with_args(program.arguments(command))
        .with_workspace(workspace.to_path_buf())
        .piped_output();
    let policy = SpawnPolicy::default().with_fence(FenceConfig {
        wall_timeout_ms: HEADER_HELPER_TIMEOUT.as_millis() as u64,
        ..FenceConfig::default()
    });
    let launcher = Arc::clone(launcher);
    let waited = spawn_blocking(move || {
        let child = launcher.spawn(request, policy)?;
        block_on(child.wait())
    });
    let exit = match timeout(HEADER_HELPER_TIMEOUT, waited).await {
        Ok(Ok(Ok(exit))) => exit,
        Ok(Ok(Err(_))) => return Err("the helper did not finish"),
        Ok(Err(_)) => return Err("the helper task did not finish"),
        Err(_) => return Err("the helper did not finish inside its budget"),
    };
    if exit.exit_code != Some(0) {
        return Err("the helper exited non-zero");
    }
    let output = exit.stdout.unwrap_or_default();
    let parsed: BTreeMap<String, String> = serde_json::from_str(output.trim())
        .map_err(|_| "the helper output is not a JSON object of strings")?;
    Ok(parsed)
}

/// The headers the transport may carry: a reserved name is skipped, and a
/// name or value the HTTP layer rejects is skipped by name only.
fn custom_headers(headers: &BTreeMap<String, String>) -> HashMap<HeaderName, HeaderValue> {
    let mut map = HashMap::new();
    for (name, value) in headers {
        if RESERVED_HEADERS
            .iter()
            .any(|reserved| reserved.eq_ignore_ascii_case(name))
        {
            tracing::warn!(
                header = %name,
                "a reserved mcp header is managed by the transport; the configured one is skipped"
            );
            continue;
        }
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => {
                map.insert(name, value);
            }
            _ => tracing::warn!(
                header = %name,
                "an mcp header name or value is not valid; it is skipped"
            ),
        }
    }
    map
}

#[cfg(test)]
mod tests;
