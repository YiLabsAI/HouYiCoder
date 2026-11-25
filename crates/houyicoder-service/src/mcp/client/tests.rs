//! Tests for this module.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;

use super::*;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::duplex;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::timeout;

/// A directory becomes the standard file URI, with spaces escaped and no
/// doubled slash.
#[test]
fn test_file_uri_unix_shape() {
    assert_eq!(file_uri_for(Path::new("/work/src")), "file:///work/src");
    assert_eq!(
        file_uri_for(Path::new("/work/my repo")),
        "file:///work/my%20repo"
    );
}

/// A Windows drive path becomes the three-slash form a client parses, and
/// backslashes are normalized rather than escaped.
#[test]
fn test_file_uri_windows_shape() {
    assert_eq!(
        file_uri_for(Path::new("C:\\work\\src")),
        "file:///C:/work/src"
    );
}

/// The environment a child receives is the base set plus the configured
/// variables, and a host variable outside both never crosses.
#[test]
fn test_child_environment_whitelist() {
    let host = vec![
        ("PATH".to_string(), "/usr/bin".to_string()),
        ("HOME".to_string(), "/home/u".to_string()),
        ("SECRET".to_string(), "leak".to_string()),
        ("TOKEN".to_string(), "host-token".to_string()),
    ];
    let mut configured = BTreeMap::new();
    configured.insert("TOKEN".to_string(), "config-token".to_string());
    configured.insert("EXTRA".to_string(), "x".to_string());
    let env = child_environment(&configured, &host);
    let get = |name: &str| {
        env.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(get("PATH"), Some("/usr/bin"));
    assert_eq!(get("HOME"), Some("/home/u"));
    assert_eq!(get("SECRET"), None, "a host secret is blocked");
    assert_eq!(
        get("TOKEN"),
        Some("config-token"),
        "the configuration wins over the host value"
    );
    assert_eq!(get("EXTRA"), Some("x"));
}

/// The spawn request carries the configured program, its arguments, the
/// workspace as its directory, and piped stdio on every stream.
#[test]
fn test_spawn_request_shape() {
    let transport = McpTransportConfig::stdio("codegraph", vec!["serve".into(), "--mcp".into()]);
    let host = vec![("PATH".to_string(), "/usr/bin".to_string())];
    let request = stdio_spawn_request(&transport, Path::new("/work"), &host);
    assert_eq!(request.program, "codegraph");
    assert_eq!(request.args, vec!["serve", "--mcp"]);
    assert_eq!(request.workspace.as_deref(), Some(Path::new("/work")));
    assert!(request.interactive);
    let env = request.env.expect("the environment is set");
    assert!(env.iter().any(|(key, _)| key == "PATH"));
}

/// The handshake identity names this product and declares the roots
/// capability, which is what a server asks for before it will answer.
#[test]
fn test_handler_info_declares_roots() {
    let (events, _rx) = unbounded_channel();
    let (handler, _roots) = McpClientHandler::new(Arc::from(Path::new("/work")), events);
    let info = ClientHandler::get_info(&handler);
    assert_eq!(info.client_info.name, "houyicoder");
    assert!(
        info.capabilities.roots.is_some(),
        "the roots capability is declared"
    );
}

/// The declared root is the workspace as a file URI, named so a server can
/// tell which directory it received.
#[test]
fn test_handler_root_is_workspace() {
    let (events, _rx) = unbounded_channel();
    let (handler, _roots) = McpClientHandler::new(Arc::from(Path::new("/work/src")), events);
    #[allow(deprecated)]
    let roots = handler.roots();
    assert_eq!(roots.roots.len(), 1);
    assert_eq!(roots.roots[0].uri, "file:///work/src");
    assert_eq!(roots.roots[0].name.as_deref(), Some("workspace"));
}

/// A change notification reaches the manager's channel, one variant per kind.
#[tokio::test]
async fn test_change_notices_reach_channel() {
    let (events, mut rx) = unbounded_channel();
    let (handler, _roots) = McpClientHandler::new(Arc::from(Path::new("/work")), events);
    handler.report(ClientEvent::ToolListChanged);
    handler.report(ClientEvent::ResourceListChanged);
    assert_eq!(rx.recv().await, Some(ClientEvent::ToolListChanged));
    assert_eq!(rx.recv().await, Some(ClientEvent::ResourceListChanged));
}

/// Both failure kinds read as the stage that failed, so a status view can
/// say whether the process or the handshake broke.
#[test]
fn test_error_display_names_stage() {
    let launch = McpClientError::Launch("no such file".into());
    assert!(format!("{launch}").contains("did not start"), "{launch}");
    assert!(format!("{launch}").contains("no such file"), "{launch}");
    let serve = McpClientError::Serve("bad frame".into());
    assert!(format!("{serve}").contains("did not start"), "{serve}");
}

/// A change notice sent after the manager dropped its receiver is a no-op:
/// the server is being torn down and nobody waits for the refetch.
#[test]
fn test_notice_after_close_quiet() {
    let (events, rx) = unbounded_channel();
    drop(rx);
    let (handler, _roots) = McpClientHandler::new(Arc::from(Path::new("/work")), events);
    handler.report(ClientEvent::ToolListChanged);
}

/// A launcher that refuses the spawn reports the launcher's reason as a
/// launch failure.
#[tokio::test]
async fn test_launch_refused() {
    struct Refuser;
    impl ProcessLauncher for Refuser {
        fn spawn(
            &self,
            _req: houyicoder_api::launcher::SpawnRequest,
            _policy: houyicoder_api::launcher::SpawnPolicy,
        ) -> Result<houyicoder_api::launcher::LauncherChild, houyicoder_api::launcher::SpawnError>
        {
            Err(houyicoder_api::launcher::SpawnError::Io("missing".into()))
        }
    }
    let launcher: Arc<dyn ProcessLauncher> = Arc::new(Refuser);
    let transport = McpTransportConfig::stdio("ghost", vec![]);
    match launch_stdio(&launcher, &transport, Arc::from(Path::new("/work"))).await {
        Ok(_) => panic!("the launch must be refused"),
        Err(error) => {
            assert!(matches!(error, McpClientError::Launch(_)), "{error:?}");
            assert!(format!("{error}").contains("missing"));
        }
    }
}

/// A launcher that hands back a child with no pipes is a launch failure, not
/// a transport that waits forever.
#[tokio::test]
async fn test_launch_without_pipes() {
    struct Pipesless;
    impl ProcessLauncher for Pipesless {
        fn spawn(
            &self,
            _req: houyicoder_api::launcher::SpawnRequest,
            _policy: houyicoder_api::launcher::SpawnPolicy,
        ) -> Result<houyicoder_api::launcher::LauncherChild, houyicoder_api::launcher::SpawnError>
        {
            let waiter: houyicoder_async::PFut<
                'static,
                Result<
                    houyicoder_api::launcher::LauncherExit,
                    houyicoder_api::launcher::SpawnError,
                >,
            > = Box::pin(async { std::future::pending().await });
            Ok(houyicoder_api::launcher::LauncherChild::new(None, waiter))
        }
    }
    let launcher: Arc<dyn ProcessLauncher> = Arc::new(Pipesless);
    let transport = McpTransportConfig::stdio("ghost", vec![]);
    match launch_stdio(&launcher, &transport, Arc::from(Path::new("/work"))).await {
        Ok(_) => panic!("the launch must be refused"),
        Err(error) => assert!(
            format!("{error}").contains("no pipes"),
            "the failure names the missing pipes: {error}"
        ),
    }
}

/// A fake peer for the handshake test: answers initialize and tools/list,
/// and after the client confirms the handshake asks for the roots and emits
/// the three change notifications. The roots answer is forwarded to the
/// caller so the test can assert what the server learned.
pub(crate) fn fake_peer(
    mut server_read: tokio::io::DuplexStream,
    mut server_write: tokio::io::DuplexStream,
    roots_seen: UnboundedSender<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut pending = Vec::new();
        let mut chunk = [0u8; 4096];
        let mut asked = false;
        while let Ok(n) = server_read.read(&mut chunk).await {
            if n == 0 {
                break;
            }
            pending.extend_from_slice(&chunk[..n]);
            while let Some(at) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=at).collect();
                let Ok(text) = String::from_utf8(line) else {
                    continue;
                };
                let Ok(frame) = serde_json::from_str::<serde_json::Value>(text.trim()) else {
                    continue;
                };
                let id = frame.get("id").cloned();
                let method = frame.get("method").and_then(|m| m.as_str()).unwrap_or("");
                if let Some(reply) = peer_reply(method, id.clone()) {
                    let mut line = serde_json::to_string(&reply).expect("serializes");
                    line.push('\n');
                    if server_write.write_all(line.as_bytes()).await.is_err() {
                        return;
                    }
                }
                if method == "notifications/initialized" && !asked {
                    asked = true;
                    if server_write
                        .write_all(peer_reverse_frames().as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                if id == Some(serde_json::json!(100)) {
                    drop(roots_seen.send(text.trim().to_string()));
                }
            }
        }
    })
}

/// The reply a fake peer sends for one request, or none for a notification.
fn peer_reply(method: &str, id: Option<serde_json::Value>) -> Option<serde_json::Value> {
    match method {
        "initialize" => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": "canned", "version": "1.0"}
            }
        })),
        "tools/list" => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"tools": []}
        })),
        "tools/call" => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"content": [{"type": "text", "text": "answer"}]}
        })),
        "resources/list" => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"resources": [
                {"uri": "file:///a", "name": "first"},
                {"uri": "file:///b", "name": "second"}
            ]}
        })),
        "resources/read" => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"contents": [{"uri": "file:///a", "text": "the body"}]}
        })),
        _ => None,
    }
}

/// The reverse traffic a fake peer sends once the handshake is confirmed: a
/// roots request and one notification per list.
fn peer_reverse_frames() -> String {
    let mut frames = String::from("{\"jsonrpc\":\"2.0\",\"id\":100,\"method\":\"roots/list\"}\n");
    for changed in [
        "notifications/tools/list_changed",
        "notifications/resources/list_changed",
        "notifications/prompts/list_changed",
    ] {
        frames.push_str(&format!(
            "{{\"jsonrpc\":\"2.0\",\"method\":\"{changed}\"}}\n"
        ));
    }
    frames
}

/// The handshake completes against an in-memory peer, and the peer's own
/// reverse traffic is answered: a roots request gets the workspace back, and
/// each change notification reaches the manager's channel.
#[tokio::test]
async fn test_handshake_over_memory() {
    const BUF: usize = 8192;
    let (client_read, server_write) = duplex(BUF);
    let (server_read, client_write) = duplex(BUF);
    let (roots_seen, mut roots_rx) = unbounded_channel::<String>();
    let peer = fake_peer(server_read, server_write, roots_seen);
    let (service, mut events, roots_served) =
        serve_over(client_read, client_write, Arc::from(Path::new("/work")))
            .await
            .expect("the handshake completes");
    let info = service.peer_info().expect("the peer reported its identity");
    assert_eq!(
        info.server_info.as_ref().map(|server| server.name.as_str()),
        Some("canned")
    );
    let roots_answer = timeout(std::time::Duration::from_secs(5), roots_rx.recv())
        .await
        .expect("the roots answer arrives")
        .expect("the channel is open");
    assert!(
        roots_answer.contains("file:///work"),
        "the server learned the workspace: {roots_answer}"
    );
    assert!(
        roots_served.load(Ordering::Relaxed),
        "the answer the server received is recorded as roots served"
    );
    let mut seen = Vec::new();
    for _ in 0..3 {
        let event = timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("a change notice arrives")
            .expect("the channel is open");
        seen.push(event);
    }
    assert_eq!(
        seen,
        vec![
            ClientEvent::ToolListChanged,
            ClientEvent::ResourceListChanged,
            ClientEvent::PromptListChanged
        ]
    );
    peer.abort();
    drop(service.cancel());
}
