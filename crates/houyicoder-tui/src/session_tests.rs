use super::*;

fn sid() -> houyicoder_protocol::frontend::SessionId {
    houyicoder_protocol::frontend::SessionId::new("s1")
}

/// The inject notification's method + params must match exactly what the
/// server's handle_session_notification reads, else mid-turn injection
/// silently no-ops.
#[test]
fn test_inject_notif_shape_matches() {
    let n = inject_notification(&sid(), "also check the logs");
    assert_eq!(n.method, "session/inject");
    let p = n.params.expect("params present");
    assert_eq!(
        p.get("text").and_then(|v| v.as_str()),
        Some("also check the logs")
    );
    assert_eq!(p.get("sessionId").and_then(|v| v.as_str()), Some("s1"));
}

/// The queue_remove notification's method + params must match what the
/// server reads to drop a queued message by text.
#[test]
fn test_queue_remove_notif_shape() {
    let n = queue_remove_notification(&sid(), "stale item");
    assert_eq!(n.method, "session/queue_remove");
    let p = n.params.expect("params present");
    assert_eq!(p.get("text").and_then(|v| v.as_str()), Some("stale item"));
}

/// The inject_child notification's method + params must match what the
/// server reads to route a steering text into a child's inbox, else
/// steering silently no-ops.
#[test]
fn test_inject_child_notif_shape() {
    let n = inject_child_notification("c1", "focus on the auth module");
    assert_eq!(n.method, "session/inject_child");
    let p = n.params.expect("params present");
    assert_eq!(p.get("childSid").and_then(|v| v.as_str()), Some("c1"));
    assert_eq!(
        p.get("text").and_then(|v| v.as_str()),
        Some("focus on the auth module")
    );
}

/// The abort-child-turn notification carries the childSid the server's
/// handle_session_notification reads. A typo in the method name or the
/// param key would make the per-turn abort silently no-op.
#[test]
fn test_cancel_child_notif_shape() {
    let n = cancel_child_turn_notification("c1");
    assert_eq!(n.method, "session/cancel_child_turn");
    let p = n.params.expect("params present");
    assert_eq!(p.get("childSid").and_then(|v| v.as_str()), Some("c1"));
}

/// A read failure (the server closed or a wire error mid-stream) must
/// surface as Done{Err} so the App clears agent_busy. The prior silent
/// return wedged the TUI on any server-side fatal. Pins the fix at the
/// effect level: the driver sends a Done{Err} carrying the read error.
#[tokio::test]
async fn test_drive_client_read_done() {
    use houyicoder_async::PFut;
    use houyicoder_client::Transport;
    use houyicoder_protocol::handshake::Hello;
    use houyicoder_protocol::wire::{WireError, WireErrorKind};

    /// A transport that serves one Hello (so connect succeeds) then fails
    /// every subsequent recv — the peer-gone condition drive_client must
    /// translate to Done{Err}.
    struct FailAfterHello {
        served: bool,
    }
    impl Transport for FailAfterHello {
        fn send_frame(&mut self, _frame: &str) -> PFut<'_, Result<(), WireError>> {
            Box::pin(async { Ok(()) })
        }
        fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, WireError>> {
            if !self.served {
                self.served = true;
                let mut h = houyicoder_protocol::framing::encode(&Hello::local()).expect("encode");
                if !h.ends_with('\n') {
                    h.push('\n');
                }
                return Box::pin(async move { Ok(Some(h)) });
            }
            Box::pin(async {
                Err(WireError::new(
                    WireErrorKind::Unavailable,
                    "peer gone",
                    false,
                ))
            })
        }
    }

    let client = houyicoder_client::Client::new(Box::new(FailAfterHello { served: false }));
    let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ClientCommand>();
    let (agent_tx, agent_rx) = std::sync::mpsc::channel::<AgentMessage>();
    drive_client(client, cmd_rx, agent_tx).await;
    let msg = agent_rx.recv().expect("a Done message on read error");
    let result = match msg {
        AgentMessage::Done { result } => result,
        _ => panic!("expected a Done message on read error"),
    };
    let e = result.expect_err("expected Err on the read-error Done");
    assert!(
        e.message.contains("connection lost"),
        "expected a connection-lost message, got: {}",
        e.message
    );
}

/// request_rename ships a RenameSessionQuery the driver forwards as a
/// RenameSession wire request. Covers the TUI-side request path (mint id
/// + send) + the driver dispatch mapping the server-contract tests cannot
/// reach (they drive the server directly, not through the TUI Session).
#[tokio::test]
async fn test_request_rename_forwards_driver() {
    let mut app = crate::composition::build_app_for_test(None);
    let sid = app.session_id.clone();
    app.session
        .as_ref()
        .expect("session wired")
        .request_rename(sid, "x".into());
    // Let the driver forward the command + the server reply arrive so the
    // dispatch mapping executes. The reply (a Status or an error) lands on
    // the agent channel; drain it so the channel drops clean.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    while app.session.as_mut().and_then(|s| s.poll()).is_some() {}
}

/// A ServerFrame::Event carrying AgentStatus is translated to
/// AgentMessage::AgentStatus by the driver, preserving every field.
#[tokio::test]
async fn test_drive_translates_agent_status() {
    use houyicoder_async::PFut;
    use houyicoder_client::Transport;
    use houyicoder_protocol::envelope::{EventEnvelope, EventSeq, ServerFrame};
    use houyicoder_protocol::frontend::event_kind::FrontendEventKind;
    use houyicoder_protocol::handshake::Hello;
    use houyicoder_protocol::wire::{WireError, WireErrorKind};

    struct AgentStatusTransport {
        served: usize,
    }
    impl Transport for AgentStatusTransport {
        fn send_frame(&mut self, _frame: &str) -> PFut<'_, Result<(), WireError>> {
            Box::pin(async { Ok(()) })
        }
        fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, WireError>> {
            self.served += 1;
            match self.served {
                1 => {
                    let mut h =
                        houyicoder_protocol::framing::encode(&Hello::local()).expect("encode");
                    if !h.ends_with('\n') {
                        h.push('\n');
                    }
                    Box::pin(async move { Ok(Some(h)) })
                }
                2 => {
                    let frame = ServerFrame::Event(EventEnvelope::new(
                        EventSeq(0),
                        FrontendEventKind::AgentStatus {
                            agent_id: "c1".into(),
                            subagent_type: "explore".into(),
                            turn: 2,
                            tokens: 150,
                            tool_uses: 3,
                            last_activity: Some("grep".into()),
                            completed: None,
                        },
                    ));
                    let mut line = houyicoder_protocol::framing::encode(&frame).expect("encode");
                    if !line.ends_with('\n') {
                        line.push('\n');
                    }
                    Box::pin(async move { Ok(Some(line)) })
                }
                _ => Box::pin(async {
                    Err(WireError::new(WireErrorKind::Unavailable, "done", false))
                }),
            }
        }
    }

    let client = houyicoder_client::Client::new(Box::new(AgentStatusTransport { served: 0 }));
    let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ClientCommand>();
    let (agent_tx, agent_rx) = std::sync::mpsc::channel::<AgentMessage>();
    drive_client(client, cmd_rx, agent_tx).await;
    let msg = agent_rx.recv().expect("AgentStatus message");
    match msg {
        AgentMessage::AgentStatus {
            agent_id,
            turn,
            tokens,
            ..
        } => {
            assert_eq!(agent_id, "c1");
            assert_eq!(turn, 2);
            assert_eq!(tokens, 150);
        }
        other => panic!("expected AgentStatus, got {other:?}"),
    }
}
