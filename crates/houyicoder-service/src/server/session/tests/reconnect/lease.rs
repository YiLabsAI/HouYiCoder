//! Reattach gating: a terminal session and a session whose lease another
//! connection holds both refuse a second serve, and release once the
//! holder disconnects.

use super::*;

/// Terminal session reattach (covers the lease guard's Cancelled/Shutdown
/// arm): a session that was aborted (Cancelled) or handed off (Shutdown)
/// refuses a reattaching serve — there is nothing to re-emit. The serve
/// returns Unavailable without touching the runner or the parked turn.
#[tokio::test]
async fn test_reattach_cancelled_session_errors() {
    let (_runner, session, host) = runner_and_host();
    // Mark the session terminal (the cancel verb does this in the full path;
    // the guard reads the same state field, so a direct flip tests the
    // refuse-reattach contract without coupling to the cancel flow).
    host.store().set_state(session, LifecycleState::Cancelled);
    let (_client_tx, server_rx) = mpsc::channel::<String>(8);
    let (server_tx, _client_rx) = mpsc::channel::<String>(8);
    let io = FrameCarrier::new(server_tx, server_rx);
    let outcome = timeout(Duration::from_secs(2), serve_session(host, session, io)).await;
    assert!(
        outcome.is_ok(),
        "serve returns (no hang) on a terminal session"
    );
    let result = outcome.expect("completed");
    assert!(result.is_err(), "terminal session refuses reattach");
    let msg = result.unwrap_err().message;
    assert!(
        msg.contains("terminal"),
        "error names the terminal state: {msg}"
    );
}

/// Occupied lease reattach (covers the lease guard's Running arm): while one
/// connection holds the lease (serve parked at an Interruption, state
/// Running), a second serve on the same session is refused — the
/// single-writer-per-session contract. The second serve returns Unavailable
/// immediately; the first serve is unaffected. After the first disconnects,
/// the lease releases to Detached and a reattach would proceed.
#[tokio::test]
async fn test_reattach_occupied_session_errors() {
    let (_runner, session, host) = runner_and_host();

    // Connection 1: drive to the Interruption so serve1 holds the lease
    // (state Running) and parks at the ask. Do NOT answer — keep it parked.
    let (client_tx1, server_rx1) = mpsc::channel::<String>(8);
    let (server_tx1, client_rx1) = mpsc::channel::<String>(8);
    let io1 = FrameCarrier::new(server_tx1, server_rx1);
    let host1 = host.clone();
    let s1 = session;
    let serve1 = tokio::spawn(async move {
        drop(serve_session(host1, s1, io1).await);
    });
    let mut client1 = Client::new(Box::new(InProcTransport::from_halves(
        client_tx1, client_rx1,
    )));
    client1.connect().await.expect("handshake 1");
    client1
        .send_request(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text {
                    text: "go".to_string(),
                }],
                disabled_skills: Default::default(),
            },
        )
        .await
        .expect("send message 1");
    for _ in 0..64 {
        match client1.next_frame().await.expect("server 1 frame") {
            ServerFrame::Event(_) => {}
            ServerFrame::Request(_) => break,
            _ => {}
        }
    }
    // serve1 is now parked at the ask; the lease is Running.
    assert_eq!(
        host.store().state(session),
        LifecycleState::Running,
        "serve1 holds the lease"
    );

    // Connection 2 (concurrent reattach): the guard sees Running and refuses
    // before any frame is exchanged, so no client is built for it.
    let (_client_tx2, server_rx2) = mpsc::channel::<String>(8);
    let (server_tx2, _client_rx2) = mpsc::channel::<String>(8);
    let io2 = FrameCarrier::new(server_tx2, server_rx2);
    let host2 = host.clone();
    let s2 = session;
    let outcome = timeout(Duration::from_secs(2), serve_session(host2, s2, io2)).await;
    assert!(
        outcome.is_ok(),
        "second serve returns (no hang) on occupied lease"
    );
    let result = outcome.expect("completed");
    assert!(
        result.is_err(),
        "occupied lease refuses a concurrent reattach"
    );
    let msg = result.unwrap_err().message;
    assert!(
        msg.contains("lease held"),
        "error names the held lease: {msg}"
    );

    // Release the first connection: serve1 returns, the lease drops to
    // Detached, and a fresh reattach would proceed again (state checks out).
    drop(client1);
    drop(serve1.await);
    assert_eq!(
        host.store().state(session),
        LifecycleState::Detached,
        "lease released after the holder disconnects"
    );
}
