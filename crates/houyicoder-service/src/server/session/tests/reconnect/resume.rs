//! The resume verdict exchange: re-emitted asks are answered on the
//! reattached connection, and each verdict (approve, deny, a second
//! Interruption after resume) lands its own arm.

use super::harness::*;
use super::*;

/// Interruption-after-resume: the resumed run lands another Interruption, so
/// resume_pending writes a fresh PendingTurn and re-emits the new ask on the
/// same connection. The first verdict carries scope=always so the consent-rule
/// arm runs at least once across the reconnect tests.
#[tokio::test]
async fn test_reconnect_resumes_interruption() {
    let tool_call = |id: &str| CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: id.into(),
            name: "approvable".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let final_text = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let responses = vec![tool_call("toolu_1"), tool_call("toolu_2"), final_text];
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ApprovableTool));
    let (runner, session, host) = parked_host(responses, tools);
    park_first_connection(&host, session).await;

    let (mut client2, serve2) = attach_client(host.clone(), session);
    client2.connect().await.expect("handshake 2");
    let mut answered_call_ids = Vec::new();
    let mut events_received = 0u32;
    for _ in 0..64 {
        match client2.next_frame().await.expect("server 2 frame") {
            ServerFrame::Event(_) => {
                events_received += 1;
            }
            ServerFrame::Request(ask) => {
                let call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected the re-emitted ask"),
                };
                answered_call_ids.push(call_id.clone());
                // First verdict carries the workspace lifetime so the consent
                // rule runs; the second is a plain once-scope approve.
                let scope = if answered_call_ids.len() == 1 {
                    ApprovalScope::Workspace
                } else {
                    ApprovalScope::Once
                };
                client2
                    .send_reverse_response(
                        ask.req_id,
                        ClientResponsePayload::Permission(ApprovalDecision {
                            call_id,
                            approved: true,
                            updated_input: None,
                            scope,
                        }),
                    )
                    .await
                    .expect("answer re-emit");
                // Both asks answered (ask1 re-emit + ask2 from
                // Interruption-after-resume) — break so the resumed run can
                // complete; serve then enters its main loop and stops sending.
                if answered_call_ids.len() == 2 {
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(
        events_received > 0,
        "fresh reattaching client must receive replayed trajectory events (cursor count 0 means full replay)"
    );
    assert_eq!(
        answered_call_ids,
        vec!["toolu_1".to_string(), "toolu_2".to_string()],
        "reattach re-emitted ask1 then the fresh ask2 from Interruption-after-resume",
    );
    await_pending_cleared(&host, session).await;
    drop(client2);
    drop(serve2.await);
    assert!(
        host.store().pending(session).is_none(),
        "parked turn cleared after the double-resume",
    );
    // The second ask never passed through the first-card path: it reached the
    // client only by being re-emitted after the resume, and the audit still
    // counts it, so a question the user answered once is never lost.
    let audit = runner.store().trajectory_head(session).summary.approvals;
    assert_eq!(
        audit.cards, 2,
        "one card per question, including the one the resume raised"
    );
}

/// An ask the resume raised and the connection then abandoned is still a card
/// in the audit: it reached the client only through the re-emit, and nothing
/// answered it, so the audit must hold the question without a verdict instead
/// of losing it.
#[tokio::test]
async fn test_unanswered_reemit_counts_card() {
    let tool_call = |id: &str| CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: id.into(),
            name: "approvable".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let responses = vec![tool_call("toolu_1"), tool_call("toolu_2")];
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ApprovableTool));
    let (runner, session, host) = parked_host(responses, tools);
    park_first_connection(&host, session).await;

    let (mut client2, serve2) = attach_client(host.clone(), session);
    client2.connect().await.expect("handshake 2");
    let mut asks = 0;
    for _ in 0..64 {
        let frame = client2.next_frame().await.expect("server 2 frame");
        if let ServerFrame::Request(ask) = frame {
            asks += 1;
            if asks == 1 {
                let call_id = match &ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => {
                        call_id.clone()
                    }
                    _ => panic!("expected a permission ask"),
                };
                client2
                    .send_reverse_response(
                        ask.req_id,
                        ClientResponsePayload::Permission(ApprovalDecision {
                            call_id,
                            approved: true,
                            updated_input: None,
                            scope: ApprovalScope::Once,
                        }),
                    )
                    .await
                    .expect("answer the first ask");
            } else {
                // The resume raised a second question; abandon it.
                break;
            }
        }
    }
    assert_eq!(asks, 2, "the resume raised a second ask");
    drop(client2);
    drop(serve2.await);

    let audit = runner.store().trajectory_head(session).summary.approvals;
    assert_eq!(audit.cards, 2, "both questions were drawn");
    assert_eq!(
        audit.unknown_outcome, 1,
        "the abandoned card has no verdict"
    );
    assert_eq!(audit.denied, 0, "abandoning a card is not a denial");
}

/// A denied verdict on reconnect (covers the PermissionVerdict::Denied audit
/// arm): the reattaching client answers deny. The runner resumes with the
/// denied decision (the tool does not execute); the second scripted turn is
/// final text, so the run completes. The audit records a Denied verdict.
#[tokio::test]
async fn test_deny_verdict_completes_run() {
    let first = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "toolu_1".into(),
            name: "approvable".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let second = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done after denial".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ApprovableTool));
    let (runner, session, host) = parked_host(vec![first, second], tools);
    let ask1_call_id = park_first_connection(&host, session).await;
    assert!(!ask1_call_id.is_empty(), "client 1 received ask1");

    // Connection 2: re-emit ask1, answer DENY. The runner resumes with the
    // denied decision; the tool is not executed; the second turn is final
    // text; the run completes. The audit trajectory records a Denied verdict.
    let (mut client2, serve2) = attach_client(host.clone(), session);
    client2.connect().await.expect("handshake 2");
    let mut answered = false;
    for _ in 0..64 {
        match client2.next_frame().await.expect("server 2 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(ask) => {
                let call_id = match ask.payload {
                    ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                    _ => panic!("expected the re-emitted ask"),
                };
                assert_eq!(call_id, ask1_call_id);
                client2
                    .send_reverse_response(
                        ask.req_id,
                        ClientResponsePayload::Permission(ApprovalDecision {
                            call_id,
                            approved: false,
                            updated_input: None,
                            scope: ApprovalScope::Once,
                        }),
                    )
                    .await
                    .expect("answer deny");
                answered = true;
                break;
            }
            _ => {}
        }
    }
    assert!(answered, "reattach re-emitted ask1 for the deny verdict");
    await_pending_cleared(&host, session).await;
    drop(client2);
    drop(serve2.await);
    // The audit trajectory carries a Denied verdict for the re-answered call.
    let denied = runner.store().trajectory_tail(session).iter().any(|ev| {
        matches!(
            ev.event,
            SessionEvent::PermissionDecision {
                verdict: PermissionVerdict::Denied,
                ..
            }
        )
    });
    assert!(denied, "denied verdict audited on reconnect-resume");
    assert!(
        host.store().pending(session).is_none(),
        "parked turn cleared after the denied-resume",
    );
}
