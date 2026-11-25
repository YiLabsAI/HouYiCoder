//! Tests for this module: the header merge and its failure branches, plus the
//! handshake over a client double, so the transport path runs without a
//! socket.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use houyicoder_api::launcher::{
    LauncherChild, LauncherExit, ProcessLauncher, SpawnError, SpawnPolicy, SpawnRequest,
};
use houyicoder_platform::tls::install_tls_provider;
use houyicoder_protocol::mcp::McpTransportConfig;
use rmcp::model::ClientJsonRpcMessage;
use rmcp::model::EmptyResult;
use rmcp::model::InitializeResult;
use rmcp::model::ListToolsResult;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerJsonRpcMessage;
use rmcp::model::ServerResult;
use rmcp::service::ClientInitializeError;
use rmcp::transport::streamable_http_client::StreamableHttpError;
use rmcp::transport::streamable_http_client::StreamableHttpPostResponse;
use sse_stream::Error as SseError;
use sse_stream::Sse;

use super::*;

/// A launcher whose child answers with one exit record.
struct ScriptedLauncher {
    exit_code: i32,
    stdout: String,
}

impl ProcessLauncher for ScriptedLauncher {
    fn spawn(&self, _req: SpawnRequest, _policy: SpawnPolicy) -> Result<LauncherChild, SpawnError> {
        let exit = LauncherExit {
            exit_code: Some(self.exit_code),
            stdout: Some(self.stdout.clone()),
            stderr: None,
        };
        Ok(LauncherChild::new(None, Box::pin(async move { Ok(exit) })))
    }
}

fn launcher(exit_code: i32, stdout: &str) -> Arc<dyn ProcessLauncher> {
    Arc::new(ScriptedLauncher {
        exit_code,
        stdout: stdout.to_string(),
    })
}

fn remote() -> McpTransportConfig {
    McpTransportConfig::http("https://example.invalid/mcp")
}

/// The helper's answer is layered over the configured headers, and wins when
/// both name the same header.
#[tokio::test]
async fn test_headers_helper_merges() {
    let mut transport = remote();
    transport.headers.insert("x-static".into(), "a".into());
    transport.headers.insert("x-token".into(), "static".into());
    transport.headers_helper = Some("print headers".into());
    let (headers, note) = request_headers(
        &launcher(0, r#"{"x-token": "helper"}"#),
        &transport,
        Path::new("/work"),
    )
    .await;
    assert_eq!(headers.get("x-static").map(String::as_str), Some("a"));
    assert_eq!(
        headers.get("x-token").map(String::as_str),
        Some("helper"),
        "the helper's answer replaces the static value"
    );
    assert!(note.is_none());
}

/// A helper that fails leaves the configured headers in place and names
/// itself, so a later authentication failure can point at it.
#[tokio::test]
async fn test_failed_helper_keeps_headers() {
    let mut transport = remote();
    transport.headers.insert("x-static".into(), "a".into());
    transport.headers_helper = Some("exit 1".into());
    let (headers, note) = request_headers(&launcher(1, ""), &transport, Path::new("/work")).await;
    assert_eq!(
        headers,
        BTreeMap::from([("x-static".to_string(), "a".to_string())])
    );
    assert!(
        note.is_some_and(|text| text.contains("header helper")),
        "the launch failure can name the helper"
    );
}

/// A helper that cannot start leaves the configured headers in place.
#[tokio::test]
async fn test_unstarted_helper_keeps_headers() {
    struct Refusing;
    impl ProcessLauncher for Refusing {
        fn spawn(
            &self,
            _req: SpawnRequest,
            _policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            Err(SpawnError::Io("no helper here".into()))
        }
    }
    let mut transport = remote();
    transport.headers.insert("x-static".into(), "a".into());
    transport.headers_helper = Some("true".into());
    let (headers, note) = request_headers(
        &(Arc::new(Refusing) as Arc<dyn ProcessLauncher>),
        &transport,
        Path::new("/work"),
    )
    .await;
    assert_eq!(headers.len(), 1);
    assert!(note.is_some());
}

/// A helper that prints something other than a JSON object is ignored.
#[tokio::test]
async fn test_garbled_helper_keeps_headers() {
    let mut transport = remote();
    transport.headers.insert("x-static".into(), "a".into());
    transport.headers_helper = Some("print noise".into());
    let (headers, note) =
        request_headers(&launcher(0, "not json"), &transport, Path::new("/work")).await;
    assert_eq!(headers.len(), 1);
    assert!(note.is_some());
}

/// A configured entry with no helper sends exactly its headers.
#[tokio::test]
async fn test_no_helper_headers() {
    let mut transport = remote();
    transport.headers.insert("x-static".into(), "a".into());
    let (headers, note) = request_headers(&launcher(0, ""), &transport, Path::new("/work")).await;
    assert_eq!(headers.len(), 1);
    assert!(note.is_none());
}

/// The transport owns the request headers it sets per request, so a
/// configured value for one of those names is dropped.
#[test]
fn test_reserved_header_skipped() {
    let headers = BTreeMap::from([
        ("Accept".to_string(), "text/plain".to_string()),
        ("x-static".to_string(), "a".to_string()),
    ]);
    let map = custom_headers(&headers);
    let names: Vec<&str> = map.keys().map(HeaderName::as_str).collect();
    assert!(!names.contains(&"accept"), "the transport owns that name");
    assert_eq!(names, vec!["x-static"]);
}

/// A transport failure that is not a challenge reads as a broken connection.
#[test]
fn test_classify_broken_link() {
    let error = ClientInitializeError::ConnectionClosed("the peer closed".into());
    match classify_launch_error(error) {
        McpClientError::Serve(reason) => assert!(reason.contains("peer closed"), "{reason}"),
        other => panic!("a closed link is not an auth failure: {other}"),
    }
}

/// Preparing a remote client resolves the headers and attaches the reconnect
/// policy without touching the network.
#[tokio::test]
async fn test_client_for_builds() {
    let mut transport = remote();
    transport.headers.insert("x-static".into(), "a".into());
    let (_, config, note) = http_client_for(&launcher(0, ""), &transport, Path::new("/work"))
        .await
        .expect("the client is built");
    let name = HeaderName::from_static("x-static");
    assert_eq!(
        config
            .custom_headers
            .get(&name)
            .and_then(|v| v.to_str().ok()),
        Some("a")
    );
    assert!(note.is_none());
}

/// An HTTP client double that answers the handshake in memory, so the rest of
/// the transport path runs without a socket.
#[derive(Clone, Default)]
struct InMemoryClient;

impl StreamableHttpClient for InMemoryClient {
    type Error = std::io::Error;

    async fn post_message(
        &self,
        _uri: Arc<str>,
        message: ClientJsonRpcMessage,
        _session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let ClientJsonRpcMessage::Request(request) = message else {
            return Ok(StreamableHttpPostResponse::Accepted);
        };
        let result = match request.request.method() {
            "initialize" => ServerResult::InitializeResult(
                InitializeResult::new(ServerCapabilities::default())
                    .with_instructions("the fake server speaks"),
            ),
            "tools/list" => ServerResult::ListToolsResult(ListToolsResult::default()),
            _ => ServerResult::EmptyResult(EmptyResult {}),
        };
        let response = ServerJsonRpcMessage::response(result, request.id);
        Ok(StreamableHttpPostResponse::Json(response, None))
    }

    async fn delete_session(
        &self,
        _uri: Arc<str>,
        _session_id: Arc<str>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        Ok(())
    }

    async fn get_stream(
        &self,
        _uri: Arc<str>,
        _session_id: Option<Arc<str>>,
        _last_event_id: Option<String>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<Sse, SseError>>,
        StreamableHttpError<Self::Error>,
    > {
        Err(StreamableHttpError::ServerDoesNotSupportSse)
    }
}

/// The resolved proxy policy is what the client is built from.
#[test]
fn test_proxy_client_policies() {
    // The pool ships without a crypto backend; installing the provider is
    // what every client build in this module expects first.
    install_tls_provider();
    assert!(
        build_proxy_client(None, None).is_ok(),
        "a direct snapshot builds"
    );
    let proxy = ProxyConfig::new(
        Some("http://127.0.0.1:3128".into()),
        None,
        "localhost,127.0.0.1".into(),
    );
    assert!(
        build_proxy_client(Some(&proxy), None).is_ok(),
        "a configured endpoint builds"
    );
    assert!(
        build_proxy_client(None, Some("automatic setup did not resolve".into())).is_err(),
        "an unusable configuration fails instead of connecting direct"
    );
}

/// A helper failure is appended to whichever launch failure follows it.
#[test]
fn test_helper_note_attached() {
    let note = Some("the header helper did not answer: it exited non-zero".to_string());
    match with_helper_note(
        McpClientError::NeedsAuth("the server requires X".into()),
        note.clone(),
    ) {
        McpClientError::NeedsAuth(reason) => {
            assert!(reason.contains("the server requires X"), "{reason}");
            assert!(reason.contains("header helper"), "{reason}");
        }
        other => panic!("the challenge is kept: {other}"),
    }
    match with_helper_note(McpClientError::Serve("closed".into()), note) {
        McpClientError::Serve(reason) => assert!(reason.contains("header helper"), "{reason}"),
        other => panic!("the failure is kept: {other}"),
    }
    assert!(matches!(
        with_helper_note(McpClientError::Launch("x".into()), None),
        McpClientError::Launch(_)
    ));
}

/// A helper whose wait fails is reported rather than read as empty output.
#[tokio::test]
async fn test_helper_wait_failure() {
    struct BrokenWait;
    impl ProcessLauncher for BrokenWait {
        fn spawn(
            &self,
            _req: SpawnRequest,
            _policy: SpawnPolicy,
        ) -> Result<LauncherChild, SpawnError> {
            let wait = async { Err(SpawnError::Io("the child vanished".into())) };
            Ok(LauncherChild::new(None, Box::pin(wait)))
        }
    }
    let mut transport = remote();
    transport.headers_helper = Some("true".into());
    let (_, note) = request_headers(
        &(Arc::new(BrokenWait) as Arc<dyn ProcessLauncher>),
        &transport,
        Path::new("/work"),
    )
    .await;
    assert!(note.is_some_and(|text| text.contains("did not finish")));
}

/// The handshake completes over a client with no network, and the launch
/// returns a service with nothing to reap.
#[tokio::test]
async fn test_handshake_over_client() {
    let config = StreamableHttpClientTransportConfig::with_uri("http://localhost/mcp");
    let live = serve_with(InMemoryClient, config, Arc::from(Path::new("/work")))
        .await
        .expect("the handshake completes");
    assert!(live.child.is_none(), "a remote server has no child to reap");
    assert!(
        live.service.peer_info().is_some(),
        "the server's identity is captured"
    );
    drop(live.service.cancel());
}
