//! The scaffolding every reconnect scenario shares: the parked-run fixture,
//! one live connection through serve_session, and the frame predicates the
//! assertions read. Kept here so a scenario file states only its own
//! behaviour instead of rebuilding the same six-object setup.

use super::*;

/// A read-only tool that keeps a resumed run active until released.
pub(super) struct ReconnectBlockingTool {
    pub(super) entered: Arc<Notify>,
    pub(super) release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
}

impl Tool for ReconnectBlockingTool {
    fn name(&self) -> &str {
        "reconnect_blocking"
    }

    fn description(&self) -> &str {
        "blocks a resumed run until the test releases it"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }

    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let entered = self.entered.clone();
        let release = self.release.clone();
        Box::pin(async move {
            entered.notify_one();
            let receiver = release.lock().expect("release lock").take();
            if let Some(receiver) = receiver {
                let _ = receiver.await;
            }
            Ok(serde_json::json!({"ok": true}))
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

pub(super) fn reconnect_user_message(frame: &ServerFrame, text: &str) -> bool {
    matches!(
        frame,
        ServerFrame::Event(EventEnvelope {
            payload: FrontendEvent::SessionUpdate {
                update: SessionUpdate::UserMessageChunk(chunk),
            },
            ..
        }) if matches!(&chunk.content, ContentBlock::Text { text: body } if body == text)
    )
}

pub(super) fn reconnect_commit(frame: &ServerFrame, input: &QueuedInput) -> bool {
    matches!(
        frame,
        ServerFrame::Event(EventEnvelope {
            payload: FrontendEvent::QueuedInputCommitted { inputs },
            ..
        }) if inputs.iter().any(|committed| committed.id == input.id)
    )
}

pub(super) fn reconnect_text_delta(frame: &ServerFrame) -> bool {
    matches!(
        frame,
        ServerFrame::Event(EventEnvelope {
            payload: FrontendEvent::Acpx { notification },
            ..
        }) if notification.method == AcpxMethod::LlmTextDelta
    )
}

pub(super) async fn await_inject_dispatch(client: &mut Client) {
    let req_id = RequestId(u64::MAX - 1);
    client
        .send_request(
            req_id,
            FrontendRequest::ChildTranscript {
                child_sid: houyicoder_protocol::frontend::SessionId::new(
                    SessionId::new().to_string(),
                ),
            },
        )
        .await
        .expect("send dispatch barrier");
    loop {
        let frame = timeout(Duration::from_secs(2), client.next_frame())
            .await
            .expect("dispatch barrier timeout")
            .expect("dispatch barrier frame");
        if matches!(frame, ServerFrame::Response(response) if response.req_id == req_id) {
            break;
        }
    }
}

/// Attach an in-memory client to a session host, returning the client and the join handle.
pub(super) fn attach_client(
    host: Arc<SessionHost>,
    session: SessionId,
) -> (Client, tokio::task::JoinHandle<()>) {
    let (client_tx, server_rx) = mpsc::channel::<String>(64);
    let (server_tx, client_rx) = mpsc::channel::<String>(64);
    let io = FrameCarrier::new(server_tx, server_rx);
    let serve = tokio::spawn(async move {
        drop(serve_session(host, session, io).await);
    });
    let client = Client::new(Box::new(InProcTransport::from_halves(client_tx, client_rx)));
    (client, serve)
}

/// Receive the re-emitted permission ask from a reattached client and approve it.
pub(super) async fn approve_reemit_ask(client: &mut Client, scope: &str) -> String {
    loop {
        let frame = timeout(Duration::from_secs(2), client.next_frame())
            .await
            .expect("reattach frame timeout")
            .expect("reattach frame");
        if let ServerFrame::Request(ask) = frame {
            let call_id = match ask.payload {
                ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                _ => panic!("expected permission request"),
            };
            client
                .send_reverse_response(
                    ask.req_id,
                    ClientResponsePayload::Permission(ApprovalDecision {
                        call_id: call_id.clone(),
                        approved: true,
                        updated_input: None,
                        scope: scope.into(),
                    }),
                )
                .await
                .expect("approve reattached request");
            return call_id;
        }
    }
}

/// Assert reconnect event ordering: user message precedes commit, commit precedes delta, seqs increase.
pub(super) fn assert_reconnect_frame_order(
    frames: &[ServerFrame],
    input: &QueuedInput,
    note: &str,
) {
    let user = frames
        .iter()
        .position(|f| reconnect_user_message(f, note))
        .expect("user input mapped");
    let committed = frames
        .iter()
        .position(|f| reconnect_commit(f, input))
        .expect("commit by stable id");
    let delta = frames
        .iter()
        .position(reconnect_text_delta)
        .expect("model delta");
    let seqs: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            ServerFrame::Event(ev) => Some(ev.seq),
            _ => None,
        })
        .collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
    assert!(user < committed, "user must precede commit");
    assert!(committed < delta, "commit must precede delta");
}

/// Encode a frame for the raw client ends that drive a channel pair without
/// the Client wrapper.
pub(super) fn reconnect_encode(msg: &impl serde::Serialize) -> String {
    let mut f = encode(msg).expect("encode");
    if !f.ends_with('\n') {
        f.push('\n');
    }
    f
}

/// Build the parked-run harness the two failure tests below share: a host
/// whose session run stops at a permission ask, plus the channel halves of
/// a reattaching connection.
pub(super) fn parked_host(
    responses: Vec<CompletionResponse>,
    tools: ToolRegistry,
) -> (Arc<Runner>, SessionId, Arc<SessionHost>) {
    let session = SessionId::new();
    let append_notify = Arc::new(Notify::new());
    let store = Arc::new(
        SessionStore::new(Box::new(InMemoryBackend::new()))
            .with_append_notify(append_notify.clone()),
    );
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(responses));
    let event_sequencer = EventSequencer::new();
    let mut runner = Runner::new(
        store,
        provider,
        tools,
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 10,
            ..RunnerConfig::default()
        },
    );
    event_sequencer.install_on(&mut runner);
    let gate: Arc<dyn houyicoder_permission::ModeGate> = Arc::new(DefaultModeGate::new());
    let runner = Arc::new(runner);
    let host = Arc::new(SessionHost::new(SessionLeaseStore::new()));
    host.insert(
        session,
        runner.clone(),
        event_sequencer,
        gate,
        append_notify,
    );
    (runner, session, host)
}

/// Drive the first connection to the parked ask, then disconnect so the
/// session is resumable by the tests' second connection.
pub(super) async fn park_first_connection(host: &Arc<SessionHost>, session: SessionId) -> String {
    let (mut client1, serve1) = attach_client(host.clone(), session);
    client1.connect().await.expect("first handshake");
    client1
        .send_request(
            RequestId(1),
            FrontendRequest::MessageSend {
                session_id: houyicoder_protocol::frontend::SessionId::new(session.to_string()),
                content: vec![ContentBlock::Text { text: "go".into() }],
                disabled_skills: Default::default(),
            },
        )
        .await
        .expect("start interrupted run");
    let call_id = loop {
        let frame = timeout(Duration::from_secs(2), client1.next_frame())
            .await
            .expect("first connection frame timeout")
            .expect("first connection frame");
        if let ServerFrame::Request(ask) = frame {
            break match ask.payload {
                ServerRequestPayload::Permission(ApprovalRequest { call_id, .. }) => call_id,
                _ => panic!("expected a permission ask"),
            };
        }
    };
    drop(client1);
    timeout(Duration::from_secs(2), serve1)
        .await
        .expect("first connection closes")
        .expect("first serve task");
    call_id
}
