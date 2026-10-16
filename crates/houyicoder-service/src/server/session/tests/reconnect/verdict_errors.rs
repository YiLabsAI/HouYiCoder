//! A reattaching connection tolerates a misbehaving peer: an unrelated
//! response is dropped rather than ending the connection, a wrong payload
//! shape fails closed, and a disconnect at any point leaves the parked turn
//! resumable.

use super::harness::*;
use super::*;

/// A disconnect before answering a re-emitted permission request preserves
/// the parked turn so another connection can resume it.
#[tokio::test]
async fn test_mid_reemit_disconnect_errors() {
    let (_runner, session, host) = runner_and_host();
    park_first_connection(&host, session).await;

    // Connection 2: receive the re-emitted ask, then drop without answering.
    let (mut client2, serve2) = attach_client(host.clone(), session);
    client2.connect().await.expect("handshake 2");
    let mut reemitted = false;
    for _ in 0..64 {
        match client2.next_frame().await.expect("server 2 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(_) => {
                reemitted = true;
                break;
            }
            _ => {}
        }
    }
    assert!(
        reemitted,
        "reattach re-emitted the ask before the disconnect"
    );
    drop(client2);
    let outcome = timeout(Duration::from_secs(2), serve2).await;
    assert!(
        outcome.is_ok(),
        "serve_session returns (no hang) on mid-re-emit disconnect"
    );
    // The parked turn survives — remaining still holds the ask, decided empty.
    let parked = host
        .store()
        .pending(session)
        .expect("parked turn survives mid-re-emit disconnect");
    assert_eq!(
        parked.remaining.len(),
        1,
        "the unanswered ask still pending"
    );
    assert!(parked.decided.is_empty(), "no verdict advanced");
}

/// A reattaching connection ignores an unrelated reverse response and keeps
/// waiting for the verdict correlated with its permission request.
#[tokio::test]
async fn test_reconnect_drops_wrong_response() {
    let (_runner, session, host) = runner_and_host();
    park_first_connection(&host, session).await;

    // Connection 2: receive the re-emitted ask, then send a response with a
    // MISMATCHED req_id (not the one the server minted for the re-emit).
    let (mut client2, serve2) = attach_client(host.clone(), session);
    client2.connect().await.expect("handshake 2");
    let mut reemit_req_id = None;
    for _ in 0..64 {
        match client2.next_frame().await.expect("server 2 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                reemit_req_id = Some(ask.req_id);
                break;
            }
            _ => {}
        }
    }
    let reemit_req_id = reemit_req_id.expect("re-emitted ask received");
    let bogus = RequestId(reemit_req_id.0 + 99);
    client2
        .send_reverse_response(
            bogus,
            ClientResponsePayload::Permission(ApprovalDecision {
                call_id: "toolu_1".to_string(),
                approved: true,
                updated_input: None,
                scope: "once".to_string(),
            }),
        )
        .await
        .expect("send mismatched response");
    client2
        .send_reverse_response(
            reemit_req_id,
            ClientResponsePayload::Permission(ApprovalDecision {
                call_id: "toolu_1".to_string(),
                approved: true,
                updated_input: None,
                scope: "once".to_string(),
            }),
        )
        .await
        .expect("send matching response");
    await_inject_dispatch(&mut client2).await;
    assert!(
        !serve2.is_finished(),
        "serve_session must continue after ignoring a mismatched response"
    );
    assert!(
        host.store().pending(session).is_none(),
        "matching verdict advances the parked turn"
    );
    drop(client2);
    drop(serve2.await);
}

/// Answering the re-emitted ask with the wrong payload shape fails the
/// resumed session closed: the verdict cannot be guessed, so a trust payload
/// against a permission ask is an InvalidFrame error.
#[tokio::test]
async fn test_reemit_wrong_payload_fails() {
    let responses = vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "toolu_approval".into(),
                name: "approvable".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done after approval".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ApprovableTool));
    let (_runner, session, host) = parked_host(responses, tools);
    park_first_connection(&host, session).await;

    let (client_tx2, server_rx2) = mpsc::channel::<String>(64);
    let (server_tx2, client_rx2) = mpsc::channel::<String>(64);
    let io2 = FrameCarrier::new(server_tx2, server_rx2);
    let host2 = host.clone();
    let serve2 = tokio::spawn(async move { serve_session(host2, session, io2).await });
    let mut client2 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx2, client_rx2,
    )));
    client2.connect().await.expect("second handshake");
    let mut ask_id = None;
    while ask_id.is_none() {
        let frame = timeout(Duration::from_secs(2), client2.next_frame())
            .await
            .expect("re-emit frame timeout")
            .expect("re-emit frame");
        if let ServerFrame::Request(req) = frame {
            ask_id = Some(req.req_id);
        }
    }
    client2
        .send_reverse_response(
            ask_id.expect("re-emitted ask"),
            ClientResponsePayload::TrustAccept(TrustAccept { accepted: true }),
        )
        .await
        .expect("send the wrong payload");
    let err = timeout(Duration::from_secs(2), serve2)
        .await
        .expect("second serve returns")
        .expect("second serve join")
        .expect_err("the wrong payload fails the resumed session closed");
    assert_eq!(err.category, ErrorCategory::InvalidFrame);
    assert_eq!(err.message, "expected a permission reverse response");
}

/// Closing the connection after approving the re-emitted ask, while the
/// resumed run is still active, fails the resumed session with a typed
/// Unavailable error rather than parking silently.
#[tokio::test]
async fn test_client_closed_during_resume() {
    let entered = Arc::new(Notify::new());
    let (release_tx, release_rx) = oneshot::channel();
    let release = Arc::new(Mutex::new(Some(release_rx)));
    let responses = vec![
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "toolu_approval".into(),
                name: "approvable".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::ToolCall {
                id: "toolu_blocking".into(),
                name: "reconnect_blocking".into(),
                input: serde_json::json!({}),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "never reached".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ApprovableTool));
    tools.register(Arc::new(ReconnectBlockingTool {
        entered: entered.clone(),
        release,
    }));
    let (_runner, session, host) = parked_host(responses, tools);
    park_first_connection(&host, session).await;

    // The second connection drives the raw channel: approve the re-emitted
    // ask, then close the send half while the resumed run blocks inside the
    // tool. The receive half stays alive so event flushes succeed and the
    // closed-send branch is the only error path.
    let (mut client_tx2, server_rx2) = mpsc::channel::<String>(64);
    let (server_tx2, mut client_rx2) = mpsc::channel::<String>(64);
    let io2 = FrameCarrier::new(server_tx2, server_rx2);
    let host2 = host.clone();
    let serve2 = tokio::spawn(async move { serve_session(host2, session, io2).await });
    client_tx2
        .send(reconnect_encode(&Hello::local()))
        .await
        .expect("send hello");
    let mut ask = None;
    while ask.is_none() {
        let line = timeout(Duration::from_secs(2), client_rx2.next())
            .await
            .expect("re-emit frame timeout")
            .expect("re-emit frame");
        if let Ok(ServerFrame::Request(req)) = serde_json::from_str(&line) {
            ask = Some(req);
        }
    }
    let ask = ask.expect("re-emitted ask");
    let call_id = match &ask.payload {
        ServerRequestPayload::Permission(a) => a.call_id.clone(),
        other => panic!("expected a permission ask, got {other:?}"),
    };
    let decision = ClientResponsePayload::Permission(ApprovalDecision {
        call_id,
        approved: true,
        updated_input: None,
        scope: "once".to_string(),
    });
    client_tx2
        .send(reconnect_encode(&ClientFrame::Response(
            ClientResponseEnvelope::new(ask.req_id, decision),
        )))
        .await
        .expect("approve the ask");

    // Effect latch: the resumed run is inside the blocking tool, so the
    // resume loop is live when the send half closes.
    timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("resumed run entered the blocking tool");
    drop(client_tx2);
    let err = timeout(Duration::from_secs(2), serve2)
        .await
        .expect("second serve returns")
        .expect("second serve join")
        .expect_err("closing during the resume fails the session");
    assert_eq!(err.category, ErrorCategory::Unavailable);
    assert_eq!(err.message, "client closed mid-resume");
    drop(release_tx);
    drop(client_rx2);
}
