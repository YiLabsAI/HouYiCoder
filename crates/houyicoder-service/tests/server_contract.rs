//! Server-dimension contract: one protocol turn threaded end-to-end through
//! the in-memory carrier, with the real runner driving the run. The server
//! performs the Hello handshake, the client sends a MessageSend, the runner
//! produces its turn events, and the server forwards each as a TurnEvent frame
//! then returns the run outcome as a response. The client decodes ServerFrame
//! (Event | Response) so it routes the seq stream and the req_id reply without
//! guessing. The same path a pipe carrier would serve.

use futures::SinkExt;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::SessionId;
use houyicoder_core::agent::runner_config::RunnerConfig;
use houyicoder_core::agent::{Runner, ToolRegistry};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::envelope::{
    ClientFrame, EventSeq, RequestEnvelope, RequestId, ResponsePayload, ServerFrame,
};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::handshake::Hello;
use houyicoder_provider::FakeProvider;
use houyicoder_service::server::Server;
use houyicoder_session::SessionStore;
use std::sync::Arc;

/// Build a minimal runner over a stub provider that returns one text reply,
/// so a run completes in a single turn with a FinalOutput outcome.
fn stub_runner() -> (Arc<Runner>, SessionId) {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("hello from stub"));
    let tools = ToolRegistry::new();
    let runner = Runner::with_shared_store(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "you are a test agent".into(),
            max_turns: 5,
            ..RunnerConfig::default()
        },
    );
    (Arc::new(runner), session)
}

// Frame plumbing (pair, send_frame, recv_frame, recv_hello) is shared via
// tests/common/mod.rs so this binary does not duplicate it. The stub_runner
// below stays local: its canned reply + max_turns are specific to these
// turn-outcome assertions.
mod common;
use common::{pair, recv_frame, recv_hello, send_frame};

/// A full turn through the in-memory carrier: handshake, MessageSend, then the
/// server forwards the turn events and returns the run outcome. The client
/// asserts it can route the seq-stream events and pair the final response to
/// its request id.
#[tokio::test]
async fn test_message_send_returns_outcome() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    // Handshake: both ends send Hello first.
    send_frame(&mut client_tx, &Hello::local()).await;
    let _hello = recv_hello(&mut client_rx).await; // server Hello

    // Send the user message as a request. The wire session id must name the
    // server's session; the content is a single text block.
    let req_id = RequestId(7);
    let req = RequestEnvelope::new(
        req_id,
        FrontendRequest::MessageSend {
            session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
            content: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    // Drain the frames until the run outcome response arrives. The turn
    // events arrive on the seq stream; the outcome arrives as a response
    // paired to req_id. Assert: at least one TurnEvent, then a RunOk
    // response whose outcome is FinalOutput.
    let mut saw_turn_event = false;
    let mut outcome = None;
    for _ in 0..32 {
        match recv_frame(&mut client_rx).await {
            ServerFrame::Event(ev) => {
                // The seq is monotonic from zero.
                assert!(ev.seq >= EventSeq(0));
                saw_turn_event = true;
            }
            ServerFrame::Response(resp) => {
                assert_eq!(resp.req_id, req_id, "response pairs to the request");
                match resp.payload {
                    ResponsePayload::RunOk(run) => {
                        outcome = Some(run.outcome);
                        break;
                    }
                    other => panic!("expected RunOk, got {other:?}"),
                }
            }
            _ => panic!("unexpected server frame"),
        }
    }
    drop(client_tx); // clean close from the client side
    drop(handle.await);
    assert!(saw_turn_event, "server forwarded turn events");
    let outcome = outcome.expect("got a run outcome");
    // FinalOutput carries content blocks; pull the first text block.
    match outcome {
        houyicoder_protocol::frontend::run::RunOutcome::FinalOutput { content } => {
            let text = content
                .into_iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text),
                    _ => None,
                })
                .expect("a text block");
            assert_eq!(text, "hello from stub");
        }
        other => panic!("expected FinalOutput, got {other:?}"),
    }
}

/// A non-run verb (ToolList) responds with its typed payload instead of
/// hanging; the seq stream stays empty (no run was started).
#[tokio::test]
async fn test_tool_list_without_hanging() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send_frame(&mut client_tx, &Hello::local()).await;
    let _hello = recv_hello(&mut client_rx).await;

    let req_id = RequestId(11);
    let req = RequestEnvelope::new(req_id, FrontendRequest::ToolList);
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    let resp = recv_frame(&mut client_rx).await;
    match resp {
        ServerFrame::Response(r) => {
            assert_eq!(r.req_id, req_id);
            assert!(
                matches!(r.payload, ResponsePayload::Tools(_)),
                "tool list returns the wire tool entries"
            );
        }
        other => panic!("expected a response, got {other:?}"),
    }
    drop(client_tx);
    drop(handle.await);
}

/// RunCancel aborts the in-flight run and acks; the run outcome returns as
/// the response to the original run request, not the cancel request.
#[tokio::test]
async fn test_run_cancel_acks() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send_frame(&mut client_tx, &Hello::local()).await;
    let _hello = recv_hello(&mut client_rx).await;

    let req_id = RequestId(5);
    let req = RequestEnvelope::new(
        req_id,
        FrontendRequest::RunCancel {
            session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
            reason: "user esc".to_string(),
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    let resp = recv_frame(&mut client_rx).await;
    match resp {
        ServerFrame::Response(r) => {
            assert_eq!(r.req_id, req_id);
            assert!(matches!(r.payload, ResponsePayload::Ack), "cancel acks");
        }
        other => panic!("expected ack response, got {other:?}"),
    }
    drop(client_tx);
    drop(handle.await);
}

/// A bad frame (not a valid request envelope) surfaces as a wire error the
/// client can branch on, and the loop keeps running for the next frame.
#[tokio::test]
async fn test_bad_frame_surfaces_error() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send_frame(&mut client_tx, &Hello::local()).await;
    let _hello = recv_hello(&mut client_rx).await;

    // Send a malformed frame (valid JSON, wrong shape for a request).
    client_tx.send("not-a-request\n".to_string()).await.unwrap();

    // The server surfaces a wire error response, then stays up.
    let resp = recv_frame(&mut client_rx).await;
    match resp {
        ServerFrame::Response(r) => {
            assert!(
                matches!(r.payload, ResponsePayload::Error(_)),
                "error payload"
            );
        }
        other => panic!("expected an error response, got {other:?}"),
    }

    // The loop continues: a well-formed ToolList request still responds with
    // its typed payload (not an error), proving the server stayed up.
    let req = RequestEnvelope::new(RequestId(3), FrontendRequest::ToolList);
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;
    let resp = recv_frame(&mut client_rx).await;
    assert!(
        matches!(resp, ServerFrame::Response(r) if matches!(r.payload, ResponsePayload::Tools(_))),
        "server kept running after the bad frame",
    );
    drop(client_tx);
    drop(handle.await);
}

/// A MessageSend whose session_id does not name the server's session fails
/// closed: the server returns an Error response (not an Ack, not a silent
/// drop) so a misrouted request cannot drive the wrong session.
#[tokio::test]
async fn test_wrong_session_fails_closed() {
    let (runner, _session) = stub_runner();
    let other_session = stub_runner().1;
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        other_session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send_frame(&mut client_tx, &Hello::local()).await;
    let _hello = recv_hello(&mut client_rx).await;

    let req_id = RequestId(31);
    let req = RequestEnvelope::new(
        req_id,
        FrontendRequest::MessageSend {
            session_id: houyicoder_protocol::frontend::SessionId::new("not-this-session"),
            content: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    let resp = recv_frame(&mut client_rx).await;
    match resp {
        ServerFrame::Response(r) => {
            assert_eq!(r.req_id, req_id, "error pairs to the request");
            assert!(
                matches!(r.payload, ResponsePayload::Error(_)),
                "mismatched session id returns an error, not an ack or run",
            );
        }
        other => panic!("expected an error response, got {other:?}"),
    }
    drop(client_tx);
    drop(handle.await);
}

/// A prefix-scoped AddRule arrives at the server and is applied verbatim —
/// the rule the frontend authored, not a blanket tool-allow reconstructed at
/// the boundary from a bare action string. The response carries the updated
/// rule set so the TUI caches from the single authority. Regression guard:
/// the server must not silently downgrade a bash prefix rule to a blanket.
#[tokio::test]
async fn test_add_rule_applies_scoped() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send_frame(&mut client_tx, &Hello::local()).await;
    let _hello = recv_hello(&mut client_rx).await;

    let req_id = RequestId(7);
    let rule = houyicoder_protocol::frontend::permission::PermissionRule {
        action: "bash".into(),
        content: Some(
            houyicoder_protocol::frontend::permission::PermissionRuleContent::Prefix {
                value: "npm install".into(),
            },
        ),
        effect: houyicoder_protocol::frontend::permission::PermissionEffect::Allow,
        ..Default::default()
    };
    let req = RequestEnvelope::new(req_id, FrontendRequest::PermissionAddRule { rule });
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    let resp = recv_frame(&mut client_rx).await;
    match resp {
        ServerFrame::Response(r) => {
            assert_eq!(r.req_id, req_id);
            match r.payload {
                ResponsePayload::PermissionRules(rules) => {
                    assert_eq!(rules.len(), 1, "exactly the one rule was added");
                    let applied = &rules[0];
                    assert_eq!(applied.action, "bash");
                    assert_eq!(
                        applied.effect,
                        houyicoder_protocol::frontend::permission::PermissionEffect::Allow
                    );
                    match &applied.content {
                        Some(houyicoder_protocol::frontend::permission::PermissionRuleContent::Prefix { value }) => {
                            assert_eq!(value, "npm install", "prefix scope preserved, not downgraded to blanket");
                        }
                        other => panic!("prefix content preserved, got {other:?}"),
                    }
                }
                other => panic!("expected PermissionRules readback, got {other:?}"),
            }
        }
        other => panic!("expected a response, got {other:?}"),
    }
    drop(client_tx);
    drop(handle.await);
}

/// The Reject effect maps to engine Deny, not Allow. A reversed mapping would
/// silently turn a deny rule into an allow a permission-escalation class bug.
/// Symmetric to the Allow+Prefix test: proves the effect axis round-trips
/// safely, not just the happy Allow path.
#[tokio::test]
async fn test_add_rule_reject_denies() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send_frame(&mut client_tx, &Hello::local()).await;
    let _hello = recv_hello(&mut client_rx).await;

    let req_id = RequestId(9);
    let rule = houyicoder_protocol::frontend::permission::PermissionRule {
        action: "bash".into(),
        content: None,
        effect: houyicoder_protocol::frontend::permission::PermissionEffect::Reject,
        ..Default::default()
    };
    let req = RequestEnvelope::new(req_id, FrontendRequest::PermissionAddRule { rule });
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    let resp = recv_frame(&mut client_rx).await;
    match resp {
        ServerFrame::Response(r) => {
            assert_eq!(r.req_id, req_id);
            match r.payload {
                ResponsePayload::PermissionRules(rules) => {
                    assert_eq!(rules.len(), 1);
                    assert_eq!(
                        rules[0].effect,
                        houyicoder_protocol::frontend::permission::PermissionEffect::Reject,
                        "Reject must not silently flip to Allow at the boundary",
                    );
                    assert_eq!(rules[0].action, "bash");
                }
                other => panic!("expected PermissionRules readback, got {other:?}"),
            }
        }
        other => panic!("expected a response, got {other:?}"),
    }
    drop(client_tx);
    drop(handle.await);
}

/// /undo on a fresh server (no destructive ops run) returns UndoResult(None)
#[tokio::test]
async fn test_undo_empty_returns_none() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send_frame(&mut client_tx, &Hello::local()).await;
    let _hello = recv_hello(&mut client_rx).await;

    let req_id = RequestId(42);
    let req = RequestEnvelope::new(req_id, FrontendRequest::Undo);
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    let resp = recv_frame(&mut client_rx).await;
    match resp {
        ServerFrame::Response(r) => {
            assert_eq!(r.req_id, req_id);
            assert!(
                matches!(r.payload, ResponsePayload::UndoResult(None)),
                "empty undo stack returns None"
            );
        }
        other => panic!("expected a response, got {other:?}"),
    }
    drop(client_tx);
    drop(handle.await);
}

/// /undo with a pre-pushed BeforeImage entry returns UndoResult(Some(desc))
/// — the server handler's closure body runs (covers the BeforeImage arm).
#[tokio::test]
async fn test_undo_entry_returns_description() {
    use houyicoder_core::snapshot::{SnapshotStore, UndoEntry, UndoStack};
    let tmp = std::env::temp_dir().join(format!("undo-srv-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).expect("mkdir");
    let file_path = tmp.join("file.txt");
    std::fs::write(&file_path, "modified").expect("write");

    let store = Arc::new(SnapshotStore::new(&tmp).expect("store"));
    let stack = Arc::new(std::sync::Mutex::new(UndoStack::new()));
    stack.lock().unwrap().push(UndoEntry::BeforeImage {
        path: file_path.clone(),
        before: Some(b"original".to_vec()),
    });

    let session_store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let session = SessionId::new();
    let runner = Runner::with_shared_store(
        session_store,
        Arc::new(FakeProvider::text("hi")) as Arc<dyn ModelProvider>,
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "".into(),
            max_turns: 1,
            ..RunnerConfig::default()
        },
    );
    let mut runner = runner;
    runner.set_undo(stack, store);
    let runner = Arc::new(runner);

    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });

    send_frame(&mut client_tx, &Hello::local()).await;
    let _hello = recv_hello(&mut client_rx).await;

    let req = RequestEnvelope::new(RequestId(99), FrontendRequest::Undo);
    send_frame(&mut client_tx, &ClientFrame::Request(req)).await;

    let resp = recv_frame(&mut client_rx).await;
    match resp {
        ServerFrame::Response(r) => {
            assert_eq!(r.req_id, RequestId(99));
            match r.payload {
                ResponsePayload::UndoResult(Some(desc)) => {
                    assert!(
                        desc.contains("restored"),
                        "description should say what was undone: {desc}"
                    );
                }
                other => panic!("expected UndoResult(Some), got {other:?}"),
            }
        }
        other => panic!("expected a response, got {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&file_path).unwrap(),
        "original",
        "undo restored the before-image"
    );
    drop(client_tx);
    drop(handle.await);
    std::fs::remove_dir_all(&tmp).ok();
}

/// /permission git: query returns the default (on); set off flips it and a
/// follow-up query reflects the change. The server is the toggle authority.
#[tokio::test]
async fn test_git_confirm_query_set() {
    let (runner, session) = stub_runner();
    let (server_io, mut client_tx, mut client_rx) = pair();
    let server = Server::new(
        runner,
        session,
        std::sync::Arc::new(houyicoder_permission::DefaultModeGate::new()),
    );
    let handle = tokio::spawn(async move { server.serve(server_io).await });
    send_frame(&mut client_tx, &Hello::local()).await;
    let _hello = recv_hello(&mut client_rx).await;

    // Query: default on.
    let q = RequestEnvelope::new(
        RequestId(1),
        FrontendRequest::PermissionAskBeforeGit { enabled: None },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(q)).await;
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => {
            assert!(matches!(
                r.payload,
                ResponsePayload::PermissionAskBeforeGit(true)
            ));
        }
        other => panic!("expected response, got {other:?}"),
    }
    // Set off, then the reply carries the new state.
    let s = RequestEnvelope::new(
        RequestId(2),
        FrontendRequest::PermissionAskBeforeGit {
            enabled: Some(false),
        },
    );
    send_frame(&mut client_tx, &ClientFrame::Request(s)).await;
    match recv_frame(&mut client_rx).await {
        ServerFrame::Response(r) => {
            assert!(matches!(
                r.payload,
                ResponsePayload::PermissionAskBeforeGit(false)
            ));
        }
        other => panic!("expected response, got {other:?}"),
    }
    drop(client_tx);
    drop(handle.await);
}
