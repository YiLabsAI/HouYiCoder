//! Card reason content, notice styling, and mid-turn config switching.

use super::*;

/// A session/set_mode arriving while a turn is live is answered in place;
/// the turn continues without reconnecting.
#[tokio::test]
async fn test_mode_switch_during_turn() {
    let session = SessionId::new();
    let (runner, entered, release_tx) = blocking_runner();
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 1)).await;
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("run entered the blocking tool");
    let echo = recv(&mut rx).await;
    assert!(echo.contains("user_message_chunk"), "prompt echo: {echo}");

    let params = json!({"sessionId": session.to_string(), "modeId": "manual"});
    send(&tx, &AcpRequest::new(2, "session/set_mode", params)).await;
    let notif = recv(&mut rx).await;
    assert!(notif.contains(r#""currentModeId":"manual""#), "{notif}");
    let reply = recv(&mut rx).await;
    assert!(reply.contains(r#""id":2"#), "set_mode reply: {reply}");

    let _ = release_tx.send(());
    let result = recv_prompt_result(&mut rx, 1).await;
    let resp: PromptResponse = serde_json::from_value(result).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// A second session/prompt arriving while a turn is live is refused with
/// InvalidRequest; the original turn is not disturbed.
#[tokio::test]
async fn test_prompt_rejected_during_turn() {
    let session = SessionId::new();
    let (runner, entered, release_tx) = blocking_runner();
    let (handle, tx, mut rx) = spawn_server(runner, session);

    send(&tx, &prompt_request(session, 1)).await;
    timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("run entered the blocking tool");
    let echo = recv(&mut rx).await;
    assert!(echo.contains("user_message_chunk"), "prompt echo: {echo}");

    send(&tx, &prompt_request(session, 2)).await;
    let rejected = recv(&mut rx).await;
    assert!(rejected.contains(r#""id":2"#), "{rejected}");
    assert!(
        rejected.contains(r#""code":-32600"#),
        "InvalidRequest code: {rejected}"
    );
    assert!(rejected.contains("a turn is already running"), "{rejected}");

    let _ = release_tx.send(());
    let result = recv_prompt_result(&mut rx, 1).await;
    let resp: PromptResponse = serde_json::from_value(result).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    drop(tx);
    handle.await.expect("join").expect("clean close");
}

/// An initialize request carrying _meta.acpx tells the server the client
/// understands extension notifications, so notices ride both channels.
#[test]
fn test_client_acpx_detected() {
    let session = SessionId::new();
    let runner = runner_with(FakeProvider::text("ok"), ToolRegistry::new());
    let mut server = AcpServer::new(
        Arc::new(AcpAdapter::new(
            AcpxCapabilities::default(),
            1,
            SessionLeaseStore::new(),
        )),
        runner,
        Arc::new(DefaultModeGate::new()),
        session,
    );
    assert!(!server.client_acpx, "starts false");
    let req = AcpRequest::new(1, "initialize", json!({"_meta": {"acpx": {}}}));
    server.note_client_acpx(&req);
    assert!(server.client_acpx, "set after acpx negotiation");
}

/// A notice sent when the client negotiated acpx reaches both the standard
/// session/update channel and the extension channel.
#[tokio::test]
async fn test_notice_sends_acpx_channel() {
    let session = SessionId::new();
    let runner = runner_with(FakeProvider::text("ok"), ToolRegistry::new());
    let mut server = AcpServer::new(
        Arc::new(AcpAdapter::new(
            AcpxCapabilities::default(),
            1,
            SessionLeaseStore::new(),
        )),
        runner,
        Arc::new(DefaultModeGate::new()),
        session,
    );
    server.client_acpx = true;
    let (_client_tx, server_rx) = mpsc::channel::<String>(16);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(16);
    let mut io = from_line_channels(server_rx, server_tx);
    server
        .push_notice(
            &mut io,
            &UserNoticeEvent {
                message: "sandbox widened".into(),
            },
        )
        .await
        .expect("push_notice");
    let first = recv(&mut client_rx).await;
    assert!(
        first.contains("agent_message_chunk"),
        "session/update first: {first}"
    );
    assert!(first.contains("sandbox widened"), "{first}");
    let second = recv(&mut client_rx).await;
    assert!(
        second.contains("_acpx/context/user_notice"),
        "acpx notification second: {second}"
    );
    assert!(second.contains("sandbox widened"), "{second}");
}

/// A notice sent when the client did not negotiate acpx reaches only the
/// standard session/update channel, not the extension channel.
#[tokio::test]
async fn test_notice_skips_acpx_channel() {
    let session = SessionId::new();
    let runner = runner_with(FakeProvider::text("ok"), ToolRegistry::new());
    let server = AcpServer::new(
        Arc::new(AcpAdapter::new(
            AcpxCapabilities::default(),
            1,
            SessionLeaseStore::new(),
        )),
        runner,
        Arc::new(DefaultModeGate::new()),
        session,
    );
    let (_client_tx, server_rx) = mpsc::channel::<String>(16);
    let (server_tx, mut client_rx) = mpsc::channel::<String>(16);
    let mut io = from_line_channels(server_rx, server_tx);
    server
        .push_notice(
            &mut io,
            &UserNoticeEvent {
                message: "directory refused".into(),
            },
        )
        .await
        .expect("push_notice");
    let first = recv(&mut client_rx).await;
    assert!(first.contains("agent_message_chunk"), "{first}");
    assert!(
        timeout(Duration::from_millis(50), client_rx.recv())
            .await
            .is_err(),
        "no second frame without acpx"
    );
}

/// A session/set_mode arriving while a permission card is pending is
/// answered in place; the card stays up and the turn resumes after the
/// verdict.
#[tokio::test]
async fn test_set_mode_during_permission() {
    let session = SessionId::new();
    let (handle, tx, mut rx) = spawn_server(guarded_runner(), session);

    send(&tx, &prompt_request(session, 1)).await;
    let ask = recv_ask(&mut rx).await;

    let params = json!({"sessionId": session.to_string(), "modeId": "manual"});
    send(&tx, &AcpRequest::new(2, "session/set_mode", params)).await;
    let notif = recv(&mut rx).await;
    assert!(notif.contains(r#""currentModeId":"manual""#), "{notif}");
    let reply = recv(&mut rx).await;
    assert!(reply.contains(r#""id":2"#), "set_mode reply: {reply}");

    send(&tx, &allow_once(ask.id.clone())).await;
    let result = recv_prompt_result(&mut rx, 1).await;
    let resp: PromptResponse = serde_json::from_value(result).expect("typed prompt response");
    assert_eq!(resp.stop_reason, StopReason::EndTurn);

    drop(tx);
    handle.await.expect("join").expect("clean close");
}
