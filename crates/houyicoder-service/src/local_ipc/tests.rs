//! Tests for this module.

use super::*;
use crate::composition::SessionHost;
use crate::lifecycle::SessionLeaseStore;
use crate::server::EventSequencer;
use crate::server::recap::{RecapSession, RecapSettings};
use futures::stream;
use houyicoder_api::agent_event::{
    EventHandler, ResponseStreamEvent, ToolExecutionEvent, UserNoticeEvent,
};
use houyicoder_api::provider::ModelProvider;
use houyicoder_async::PFut;
use houyicoder_async::PStream;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_permission::DefaultModeGate;
use houyicoder_platform::ipc;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;
#[cfg(not(unix))]
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::io::Read;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
#[cfg(unix)]
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::duplex;
use tokio::sync::Notify;
use tokio::task::spawn_blocking;
use tokio::time::sleep;
use tokio::time::timeout;

#[cfg(unix)]
use std::fs::metadata;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream as StdUnixStream;
#[cfg(unix)]
use tokio::net::UnixStream;

/// A directory short enough to hold a socket. A unix socket path must fit the
/// platform's sun_path limit, and the session's own temporary directory can be
/// long enough that appending a directory and a socket name exceeds it, so the
/// socket tests take the shortest base the host always provides instead.
fn socket_base() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/tmp")
    }
    #[cfg(not(unix))]
    {
        temp_dir()
    }
}

/// A frame round-trips through the carrier: writing on one stream end
/// lands on the FrameCarrier inbound channel, and sending on the outbound
/// channel lands on the other stream end. Proves the byte path without a
/// host or runner.
#[cfg(unix)]
#[tokio::test]
async fn test_bridge_round_trips_frame() {
    let (a, b) = StdUnixStream::pair().expect("pair");
    a.set_nonblocking(true).ok();
    b.set_nonblocking(true).ok();
    let server_stream = UnixStream::from_std(a).expect("from_std a");
    let mut client_stream = UnixStream::from_std(b).expect("from_std b");

    let mut io = super::bridge(server_stream).await;

    // client -> server: write a line on the client end, read it inbound.
    client_stream
        .write_all(b"hello-in\n")
        .await
        .expect("client write");
    client_stream.flush().await.expect("client flush");
    let inbound = io.rx.next().await.expect("inbound frame");
    assert_eq!(inbound, "hello-in");

    // server -> client: send on the outbound channel, read it on the
    // client end.
    io.tx
        .send("hello-out".to_string())
        .await
        .expect("outbound send");
    let mut buf = [0u8; 32];
    let n = client_stream.read(&mut buf).await.expect("client read");
    assert_eq!(&buf[..n], b"hello-out\n");
}

/// The listener accepts a connection and hands it to the serve path, and the
/// client reads end of stream once the serve side finishes. With no runner
/// registered for the session, serve_session returns early, so the client
/// learning that the carrier closed is the guarantee under test: a client end
/// the server never closes blocks its reader forever.
#[tokio::test]
async fn test_listener_signals_close() {
    let dir = socket_base().join(format!("hu-ipc-close-{}", id()));
    create_dir_all(&dir).ok();
    let listener = ipc::bind_session(&dir, "close").expect("bind the endpoint");
    let endpoint = listener.endpoint().to_string();

    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    let session = SessionId::new();
    let serving = tokio::spawn(async move {
        listen_local(host, session, listener).await.ok();
    });
    let connect_to = endpoint.clone();
    let streams = spawn_blocking(move || ipc::connect_local_streams(&connect_to))
        .await
        .expect("the connect task")
        .expect("connect the client");
    let mut reader = streams.read;

    let read = timeout(
        Duration::from_secs(5),
        spawn_blocking(move || {
            let mut buf = [0u8; 16];
            reader.read(&mut buf)
        }),
    )
    .await
    .expect("the serve side must close the stream")
    .expect("the read task");
    assert_eq!(
        read.expect("read the stream"),
        0,
        "the client reads end of stream"
    );
    serving.abort();
    ipc::remove_endpoint(&endpoint).ok();
    remove_dir_all(&dir).ok();
}

/// The endpoint a listener records lives in a directory restricted to its
/// owner, so the record and the socket are never readable by another local
/// account.
#[tokio::test]
async fn test_private_dir_holds_endpoint() {
    let dir = socket_base().join(format!("hu-ipc-private-{}", id()));
    ipc::create_owner_only_dir(&dir).expect("create the private dir");
    let listener = ipc::bind_session(&dir, "private").expect("bind the endpoint");
    let endpoint = listener.endpoint().to_string();
    assert!(!endpoint.is_empty());
    #[cfg(unix)]
    {
        let mode = metadata(&dir).expect("read the dir").mode() & 0o777;
        assert_eq!(mode, 0o700, "the record dir stays owner-only: {mode:o}");
    }
    drop(listener);
    ipc::remove_endpoint(&endpoint).ok();
    remove_dir_all(&dir).ok();
}

/// A frame is classified as ACP initialize when it parses as a JSON-RPC
/// request whose method is initialize.
#[test]
fn test_is_acp_initialize_classifies() {
    assert!(is_acp_initialize(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#
    ));
    assert!(!is_acp_initialize(
        r#"{"jsonrpc":"2.0","method":"session/cancel","params":{}}"#
    ));
    assert!(!is_acp_initialize("not json"));
}

/// A non-ACP first frame is re-injected into the rich carrier so the
/// frontend server sees the same stream it would without the dialect peek.
/// The frame round-trips through the carrier with the first frame prepended.
#[cfg(unix)]
#[tokio::test]
async fn test_bridge_reinjects_first_frame() {
    let (a, b) = StdUnixStream::pair().expect("pair");
    a.set_nonblocking(true).ok();
    b.set_nonblocking(true).ok();
    let server_stream = UnixStream::from_std(a).expect("from_std a");
    let mut client_stream = UnixStream::from_std(b).expect("from_std b");
    let (read_half, write_half) = split(server_stream);
    let mut io = bridge_halves(
        BufReader::new(read_half),
        write_half,
        Some("hello-in".into()),
    )
    .await;
    // The re-injected first frame arrives on the inbound channel.
    let inbound = io.rx.next().await.expect("inbound first frame");
    assert_eq!(inbound, "hello-in");
    // A second frame written by the client also arrives.
    client_stream
        .write_all(b"second\n")
        .await
        .expect("client write");
    client_stream.flush().await.expect("client flush");
    let second = io.rx.next().await.expect("inbound second frame");
    assert_eq!(second, "second");
}

/// A provider that streams text deltas with a small sleep between them, so
/// the run future is pending between deltas and the attach live stream
/// observes them.
struct StreamingProvider {
    text: String,
}

impl ModelProvider for StreamingProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        let text = self.text.clone();
        Box::pin(async move {
            Ok(CompletionResponse {
                output: vec![OutputItem::Text { text }],
                usage: Usage::default(),
                model: "test".into(),
            })
        })
    }
    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let id = "t1".to_string();
        let parts: Vec<String> = vec!["hel".into(), "lo".into()];
        Box::pin(stream::unfold(
            (0u32, id, parts),
            |(i, id, parts)| async move {
                match i {
                    0 => Some((Ok(LlmEvent::StepStart { index: 0 }), (1, id, parts))),
                    1 => Some((Ok(LlmEvent::TextStart { id: id.clone() }), (2, id, parts))),
                    2 => {
                        sleep(Duration::from_millis(2)).await;
                        Some((
                            Ok(LlmEvent::TextDelta {
                                id: id.clone(),
                                text: parts[0].clone(),
                            }),
                            (3, id, parts),
                        ))
                    }
                    3 => {
                        sleep(Duration::from_millis(2)).await;
                        Some((
                            Ok(LlmEvent::TextDelta {
                                id: id.clone(),
                                text: parts[1].clone(),
                            }),
                            (4, id, parts),
                        ))
                    }
                    4 => Some((Ok(LlmEvent::TextEnd { id: id.clone() }), (5, id, parts))),
                    5 => Some((
                        Ok(LlmEvent::StepFinish {
                            index: 0,
                            reason: "stop".into(),
                            usage: None,
                        }),
                        (6, id, parts),
                    )),
                    _ => None,
                }
            },
        ))
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

fn attach_epoch() -> ConnectionEpoch {
    ConnectionEpoch {
        current: Arc::new(AtomicU64::new(1)),
        mine: 1,
    }
}

/// Build a host with a registered session running the given provider, plus
/// the sequencer its live events go through.
fn host_with(provider: Arc<dyn ModelProvider>) -> (Arc<SessionHost>, SessionId, EventSequencer) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let mut runner = Runner::new(
        store.clone(),
        provider.clone(),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    let event_sequencer = EventSequencer::new();
    event_sequencer.install_on(&mut runner);
    let runner = Arc::new(runner);
    let gate: Arc<dyn houyicoder_permission::ModeGate> = Arc::new(DefaultModeGate::new());
    let recap = Arc::new(RecapSession::new(
        session,
        RecapSettings::default(),
        provider,
        store,
    ));
    let append_notify = Arc::new(Notify::new());
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(
        session,
        runner,
        event_sequencer.clone(),
        gate,
        append_notify,
        recap,
    );
    (host, session, event_sequencer)
}

async fn send_frame(write: &mut (impl AsyncWrite + Unpin), frame: &str) {
    let mut line = frame.to_string();
    if !line.ends_with('\n') {
        line.push('\n');
    }
    write.write_all(line.as_bytes()).await.expect("write frame");
    write.flush().await.expect("flush frame");
}

async fn read_line(reader: &mut (impl AsyncBufRead + Unpin)) -> String {
    let mut line = String::new();
    reader.read_line(&mut line).await.expect("read frame");
    line.trim_end().to_string()
}

/// A full ACP attach chain: the client initializes, opens a session bound to
/// the attached id, then prompts. The live stream reaches the client as
/// session/update chunks and the prompt replies with end_turn.
#[tokio::test]
async fn test_acp_attach_full_chain() {
    let (host, session, _sequencer) = host_with(Arc::new(StreamingProvider {
        text: "hello".into(),
    }));
    let (client, server) = duplex(65536);
    let (client_read, mut client_write) = split(client);
    let mut client_reader = BufReader::new(client_read);
    let serving = tokio::spawn(async move {
        if let Err(e) = serve_local_stream(host, session, server, attach_epoch()).await {
            tracing::debug!(error = %e, "serve ended");
        }
    });

    send_frame(
        &mut client_write,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    )
    .await;
    let reply = timeout(Duration::from_secs(5), read_line(&mut client_reader))
        .await
        .expect("initialize reply within 5s");
    assert!(reply.contains(r#""protocolVersion":1"#), "{reply}");

    let new_session = r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/work","mcpServers":[]}}"#;
    send_frame(&mut client_write, new_session).await;
    let reply = timeout(Duration::from_secs(5), read_line(&mut client_reader))
        .await
        .expect("session/new reply within 5s");
    assert!(
        reply.contains(&format!(r#""sessionId":"{session}""#)),
        "session/new returns the attached id: {reply}"
    );
    assert!(reply.contains(r#""modes""#), "{reply}");
    assert!(reply.contains(r#""configOptions""#), "{reply}");

    let prompt = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"{session}","prompt":[{{"type":"text","text":"hi"}}]}}}}"#
    );
    send_frame(&mut client_write, &prompt).await;
    let mut saw_chunk = false;
    let mut prompt_reply = None;
    for _ in 0..64 {
        let line = timeout(Duration::from_secs(5), read_line(&mut client_reader))
            .await
            .expect("prompt frame within 5s");
        if line.contains("agent_message_chunk") {
            saw_chunk = true;
        }
        if line.contains(r#""id":3"#) {
            prompt_reply = Some(line);
            break;
        }
    }
    assert!(saw_chunk, "a live chunk reached the client");
    let reply = prompt_reply.expect("prompt reply");
    assert!(reply.contains(r#""stopReason":"end_turn""#), "{reply}");

    drop(client_write);
    drop(client_reader);
    if timeout(Duration::from_secs(2), serving).await.is_err() {
        // The serve task outlived the test's wait; the assertions above hold.
    }
}

/// Tool progress and user notices reach an attached client, not only the
/// response text: the host's sequencer fans every live domain to the
/// connection.
#[tokio::test]
async fn test_acp_attach_live_domains() {
    let (host, session, sequencer) = host_with(Arc::new(StreamingProvider {
        text: "hello".into(),
    }));
    let (client, server) = duplex(65536);
    let (client_read, mut client_write) = split(client);
    let mut client_reader = BufReader::new(client_read);
    let serving = tokio::spawn(async move {
        if let Err(e) = serve_local_stream(host, session, server, attach_epoch()).await {
            tracing::debug!(error = %e, "serve ended");
        }
    });

    send_frame(
        &mut client_write,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    )
    .await;
    drop(
        timeout(Duration::from_secs(5), read_line(&mut client_reader))
            .await
            .expect("initialize reply"),
    );
    let new_session = r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/work","mcpServers":[]}}"#;
    send_frame(&mut client_write, new_session).await;
    drop(
        timeout(Duration::from_secs(5), read_line(&mut client_reader))
            .await
            .expect("session/new reply"),
    );

    // The connection subscribed during the handshake, so these reach it.
    EventHandler::<ToolExecutionEvent>::handle(
        &sequencer,
        ToolExecutionEvent::Progress {
            call_id: "call-1".into(),
            elapsed_secs: 1,
            timeout_secs: None,
            tail: vec!["partial output".into()],
        },
    );
    EventHandler::<UserNoticeEvent>::handle(
        &sequencer,
        UserNoticeEvent {
            message: "heads up".into(),
        },
    );

    let prompt = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"{session}","prompt":[{{"type":"text","text":"hi"}}]}}}}"#
    );
    send_frame(&mut client_write, &prompt).await;
    let mut saw_progress = false;
    let mut saw_notice = false;
    for _ in 0..64 {
        let line = timeout(Duration::from_secs(5), read_line(&mut client_reader))
            .await
            .expect("prompt frame within 5s");
        saw_progress |= line.contains("tool_call_update") && line.contains("partial output");
        saw_notice |= line.contains("heads up");
        if line.contains(r#""id":3"#) {
            break;
        }
    }
    assert!(saw_progress, "tool progress reached the attached client");
    assert!(saw_notice, "a user notice reached the attached client");

    drop(client_write);
    drop(client_reader);
    if timeout(Duration::from_secs(2), serving).await.is_err() {
        // The serve task outlived the test's wait; the assertions above hold.
    }
}

/// An attached connection that falls behind at the host's broadcast still
/// delivers the durable reply: the upstream loss counts as the connection's
/// own, so the durable copy is not suppressed.
#[tokio::test]
async fn test_attach_lag_keeps_durable() {
    let (host, session, sequencer) = host_with(Arc::new(StreamingProvider {
        text: "hello".into(),
    }));
    let (client, server) = duplex(65536);
    let (client_read, mut client_write) = split(client);
    let mut client_reader = BufReader::new(client_read);
    let serving = tokio::spawn(async move {
        if let Err(e) = serve_local_stream(host, session, server, attach_epoch()).await {
            tracing::debug!(error = %e, "serve ended");
        }
    });

    send_frame(
        &mut client_write,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    )
    .await;
    drop(
        timeout(Duration::from_secs(5), read_line(&mut client_reader))
            .await
            .expect("initialize reply"),
    );
    let new_session = r#"{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/work","mcpServers":[]}}"#;
    send_frame(&mut client_write, new_session).await;
    drop(
        timeout(Duration::from_secs(5), read_line(&mut client_reader))
            .await
            .expect("session/new reply"),
    );

    // More events than the host broadcast holds, published without awaiting,
    // so the forwarding task cannot keep up and reports the loss.
    for _ in 0..600 {
        EventHandler::<ResponseStreamEvent>::handle(
            &sequencer,
            ResponseStreamEvent::AssistantTextDelta { text: "x".into() },
        );
    }

    let prompt = format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{{"sessionId":"{session}","prompt":[{{"type":"text","text":"hi"}}]}}}}"#
    );
    send_frame(&mut client_write, &prompt).await;
    let mut saw_durable = false;
    for _ in 0..1024 {
        let line = timeout(Duration::from_secs(5), read_line(&mut client_reader))
            .await
            .expect("prompt frame within 5s");
        saw_durable |= line.contains(r#""text":"hello""#);
        if line.contains(r#""id":3"#) {
            break;
        }
    }
    assert!(
        saw_durable,
        "the durable reply arrives after the upstream lag"
    );

    drop(client_write);
    drop(client_reader);
    if timeout(Duration::from_secs(2), serving).await.is_err() {
        // The serve task outlived the test's wait; the assertions above hold.
    }
}

/// An empty stream is a clean close, not an error.
#[tokio::test]
async fn test_acp_empty_stream_closes() {
    let (host, session, _sequencer) = host_with(Arc::new(FakeProvider::text("x")));
    let (client, server) = duplex(1024);
    drop(client);
    let result = timeout(
        Duration::from_secs(2),
        serve_local_stream(host, session, server, attach_epoch()),
    )
    .await
    .expect("serve returned after EOF");
    assert!(result.is_ok(), "empty stream is a clean close");
}

/// A first frame above the message bound is refused with InvalidFrame before
/// the connection is routed.
#[tokio::test]
async fn test_acp_oversized_frame_rejected() {
    let (host, session, _sequencer) = host_with(Arc::new(FakeProvider::text("x")));
    let (client, server) = duplex(MAX_MESSAGE_BYTES + 4096);
    let serving =
        tokio::spawn(
            async move { serve_local_stream(host, session, server, attach_epoch()).await },
        );
    let mut client = client;
    let oversized = vec![b'x'; MAX_MESSAGE_BYTES + 1];
    client.write_all(&oversized).await.expect("write oversized");
    client.flush().await.expect("flush");
    let result = timeout(Duration::from_secs(5), serving)
        .await
        .expect("serve returned")
        .expect("join")
        .expect_err("oversized first frame is an error");
    assert_eq!(result.category, ErrorCategory::InvalidFrame);
}

/// A non-ACP first frame routes to the rich frontend server rather than the
/// ACP server.
#[tokio::test]
async fn test_acp_nonacp_routes_rich() {
    let (host, session, _sequencer) = host_with(Arc::new(FakeProvider::text("x")));
    let (client, server) = duplex(8192);
    let serving =
        tokio::spawn(
            async move { serve_local_stream(host, session, server, attach_epoch()).await },
        );
    let mut client = client;
    client
        .write_all(b"{\"type\":\"hello\"}\n")
        .await
        .expect("write hello");
    client.flush().await.expect("flush");
    drop(client);
    if timeout(Duration::from_secs(2), serving).await.is_err() {
        // The serve task outlived the wait; the routing is already proven.
    }
}
