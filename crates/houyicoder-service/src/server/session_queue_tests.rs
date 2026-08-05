//! Wire-level proofs that the server's queued_input is only the current run's
//! injection buffer: an interrupt or a /clear (SessionReset) invalidates it,
//! so an orphaned text must not leak into the next run. Split out of
//! session_tests so that file stays under the size gate. Inherits the test
//! harness (SessionHost, serve_session, NoopTool, runner_with_noop) via the
//! parent tests module.
//!
//! Placement note: these drive a real Server + serve loop + wire frames, so
//! by the AGENTS.md classification gate they are integration tests that
//! belong in tests/ rather than src/ inline. They are kept inline (inherited
//! from session_tests, not a new choice) to reuse the parent's private
//! harness. The size-gate split that produced this file pointed at "tests on
//! the wrong side"; splitting silenced that signal rather than heeding it.
//! A future refactor should move the shared harness to a test-support module
//! under tests/ so these can move with it.

use super::*;
use std::sync::atomic::AtomicU64;

/// A provider whose first stream is pending (the run hangs mid-stream so a
/// cancel can land), then returns scripted responses for later calls. For the
/// inject-then-cancel test: run 1 hangs + is aborted; run 2 completes with a
/// tool-call turn so the turn-2 drain would surface an orphaned inject if the
/// interrupt did not clear the server queue.
struct HangFirstThenScript {
    responses: Mutex<VecDeque<CompletionResponse>>,
    hung: std::sync::atomic::AtomicBool,
}
impl HangFirstThenScript {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            hung: std::sync::atomic::AtomicBool::new(false),
        }
    }
}
impl ModelProvider for HangFirstThenScript {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        unreachable!("the drive loop consumes the stream path")
    }
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    fn stream(
        &self,
        _req: CompletionRequest,
    ) -> PStream<'_, Result<houyicoder_protocol::llm::LlmEvent, ProviderError>> {
        let first = !self.hung.swap(true, std::sync::atomic::Ordering::SeqCst);
        if first {
            // Pure pending: the run stays mid-stream until the cancel fires
            // the run token, which makes model_call_stream return None.
            return Box::pin(futures::stream::pending());
        }
        let r = self
            .responses
            .lock()
            .expect("script mutex")
            .pop_front()
            .expect("scripted response");
        stream_from_response(r)
    }
}

/// A session/inject sent mid-run + then a session/cancel must NOT let the
/// injected text leak into the next run. The host's pending queue is the
/// single truth source; the server queue is the current run's injection
/// buffer only, so an interrupted run must drop the orphaned text. Without
/// the clear-on-interrupt, the text survives in queued_input; the next run's
/// turn-2 drain injects it as a user message the user never re-sent. This is
/// the wire-level proof of the layer the host-side park gate cannot see.
#[tokio::test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
async fn test_inject_cancel_run_clear() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    // Run 2's responses: a tool call (turn 1 -> RunAgain) then final text
    // (turn 2, where the drain would surface the orphan).
    let run2_first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "toolu_1".into(),
            name: "noop".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let run2_second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn ModelProvider> =
        Arc::new(HangFirstThenScript::new(vec![run2_first, run2_second]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(NoopTool));
    let runner = Arc::new(Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    ));
    let gate: Arc<dyn houyicoder_permission::ModeGate> = Arc::new(DefaultModeGate::new());
    let next_seq = Arc::new(AtomicU64::new(0));
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(session, runner.clone(), next_seq, gate);

    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));

    let host_clone = host.clone();
    let sess = session;
    tokio::spawn(async move {
        drop(serve_session(host_clone, sess, server_io).await);
    });
    client.connect().await.expect("handshake");

    // Run 1: starts, turn-1 stream hangs mid-flight.
    let run1_id = RequestId(1);
    client
        .send_request(
            run1_id,
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
            },
        )
        .await
        .expect("send run 1");
    // Let the run enter the pending stream before the inject + cancel land.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    // Inject mid-run: the mid-run select enqueues the text for the next turn
    // boundary (which the abort never reaches).
    client
        .send_notification(houyicoder_protocol::acp_wire::AcpNotification::new(
            "session/inject",
            serde_json::json!({ "sessionId": session.to_string(), "text": "orphan note" }),
        ))
        .await
        .expect("send inject");
    // Cancel: abort the run. The drive loop's model_call_stream returns None
    // -> Interrupted, without reaching the turn-2 drain.
    client
        .send_notification(houyicoder_protocol::acp_wire::AcpNotification::new(
            "session/cancel",
            serde_json::json!({ "sessionId": session.to_string() }),
        ))
        .await
        .expect("send cancel");
    // Run 1 resolves Interrupted (RunOk carries the Interrupted outcome).
    let mut run1_done = false;
    for _ in 0..64 {
        match client.next_frame().await.expect("server frame") {
            ServerFrame::Response(resp) if resp.req_id == run1_id => {
                run1_done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(run1_done, "run 1 resolved on cancel");

    // Run 2: a tool-call turn (RunAgain) then final. Turn 2 drains the
    // server queue. If the orphan survived the interrupt, QueueConsumed
    // reports it here.
    let run2_id = RequestId(2);
    client
        .send_request(
            run2_id,
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "again".to_string(),
                }],
            },
        )
        .await
        .expect("send run 2");
    let mut run2_done = false;
    let mut orphan_replayed = false;
    for _ in 0..64 {
        match client.next_frame().await.expect("server frame") {
            ServerFrame::Event(ev) => {
                if matches!(
                    ev.payload,
                    houyicoder_protocol::frontend::FrontendEventKind::QueueConsumed {
                        ref texts
                    } if texts.contains(&"orphan note".to_string())
                ) {
                    orphan_replayed = true;
                }
            }
            ServerFrame::Response(resp) if resp.req_id == run2_id => {
                run2_done = true;
                break;
            }
            _ => {}
        }
    }
    assert!(run2_done, "run 2 completed");
    assert!(
        !orphan_replayed,
        "an interrupted run must clear the server queue; the next run must not \
         re-inject the orphaned text"
    );
}

/// A session/inject that lands between runs, followed by a /clear
/// (SessionReset), must not let the injected text survive into the next run.
/// The host is the single truth source; a state-changing command invalidates
/// the server's injection buffer. Without clear_input_queue wired into the
/// reset dispatch, the text survives the reset + the next run's turn-2 drain
/// injects it into the freshly cleared context (the user's clear-the-context
/// intent defeated by a pre-clear message).
#[tokio::test]
async fn test_inject_reset_run_clear() {
    let (_runner, session, host) = runner_with_noop();
    let (client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, client_rx) = mpsc::channel::<String>(8);
    let server_io = ServerIo::new(server_tx, server_rx);
    let client_transport = InProcTransport::from_halves(client_tx, client_rx);
    let mut client = Client::new(Box::new(client_transport));

    let host_clone = host.clone();
    let sess = session;
    tokio::spawn(async move {
        drop(serve_session(host_clone, sess, server_io).await);
    });
    client.connect().await.expect("handshake");

    // Inject while no run is active: the between-runs loop enqueues it for
    // the next run.
    client
        .send_notification(houyicoder_protocol::acp_wire::AcpNotification::new(
            "session/inject",
            serde_json::json!({ "sessionId": session.to_string(), "text": "pre-clear note" }),
        ))
        .await
        .expect("send inject");

    // /clear: the reset must drop the queued text (a state-changing command
    // invalidates the injection buffer).
    let reset_id = RequestId(7);
    client
        .send_request(
            reset_id,
            FrontendRequest::SessionReset {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
            },
        )
        .await
        .expect("send reset");
    for _ in 0..16 {
        match client.next_frame().await.expect("server frame") {
            ServerFrame::Response(resp) if resp.req_id == reset_id => break,
            _ => {}
        }
    }

    // Run: a tool-call turn (RunAgain) then final. Turn 2 drains the server
    // queue. If the pre-clear text survived the reset, QueueConsumed reports
    // it here.
    let req_id = RequestId(8);
    client
        .send_request(
            req_id,
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
            },
        )
        .await
        .expect("send message");
    let mut got_run_ok = false;
    let mut orphan_replayed = false;
    for _ in 0..64 {
        match client.next_frame().await.expect("server frame") {
            ServerFrame::Event(ev) => {
                if matches!(
                    ev.payload,
                    houyicoder_protocol::frontend::FrontendEventKind::QueueConsumed {
                        ref texts
                    } if texts.contains(&"pre-clear note".to_string())
                ) {
                    orphan_replayed = true;
                }
            }
            ServerFrame::Response(resp) if resp.req_id == req_id => {
                got_run_ok = true;
                break;
            }
            _ => {}
        }
    }
    assert!(got_run_ok, "run completed after reset");
    assert!(
        !orphan_replayed,
        "a reset must drop the pre-clear injected text; the next run must not \
         re-inject it into the cleared context"
    );
}
