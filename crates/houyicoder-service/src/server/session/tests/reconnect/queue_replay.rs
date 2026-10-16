//! Queued input on a resumed run: it is replayed into the transcript and
//! committed by stable id before the next model delta.

use super::harness::*;
use super::*;

/// A queued input on a resumed run is replayed before the next model delta.
#[tokio::test]
async fn test_reconnect_replays_queued_input() {
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
                text: "after queued input".into(),
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

    let (mut client2, serve2) = attach_client(host.clone(), session);
    client2.connect().await.expect("second handshake");
    approve_reemit_ask(&mut client2, "once").await;

    timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("resumed run entered blocking tool");
    let input = QueuedInput::new("reconnected note");
    client2
        .send_notification(AcpNotification::new(
            "session/inject",
            serde_json::json!({ "input": input }),
        ))
        .await
        .expect("inject queued input");
    await_inject_dispatch(&mut client2).await;
    release_tx.send(()).expect("release blocking tool");

    let mut frames = Vec::new();
    for _ in 0..256 {
        let frame = timeout(Duration::from_secs(2), client2.next_frame())
            .await
            .expect("ordered reconnect frames timeout")
            .expect("ordered reconnect frame");
        let done = reconnect_text_delta(&frame);
        frames.push(frame);
        if done {
            break;
        }
    }
    assert_reconnect_frame_order(&frames, &input, "reconnected note");

    drop(client2);
    timeout(Duration::from_secs(2), serve2)
        .await
        .expect("second connection closes")
        .expect("second serve task");
}
