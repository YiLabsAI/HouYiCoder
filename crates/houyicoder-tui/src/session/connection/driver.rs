//! The connection driver task: translates server frames into application
//! messages and client commands into outbound frames. Spawned and owned by
//! SessionConnection; durable history remains owned by the event loop.

use std::collections::VecDeque;
use std::sync::mpsc;

use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::{
    ClientResponsePayload, RequestId, ResponsePayload, ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::frontend::FrontendEvent;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::trust::TrustAccept;
use houyicoder_protocol::frontend::{PendingInputId, QueuedInput, SessionId as FrontendSessionId};

use crate::agent_message::{
    ClientCommand, ConnectionEvent, ServerEvent, ServerRequest, ServerResponse, SessionMessage,
};
use crate::transcript::TranscriptFrame;

/// How many unknown-method notices one connection writes to the diagnostic
/// log. The notification is dropped either way; the bound keeps a peer that
/// renamed a token-level method from turning the log into a token-rate
/// stream. It is per connection and shared across methods, so a chatty
/// unknown method spends the budget for a rarer one.
struct UnknownReportBudget(u32);

impl UnknownReportBudget {
    const LIMIT: u32 = 3;

    fn new() -> Self {
        Self(0)
    }

    /// Takes one notice from the budget; false once the bound is spent.
    fn grant(&mut self) -> bool {
        if self.0 >= Self::LIMIT {
            return false;
        }
        self.0 += 1;
        true
    }
}

/// A queued outbound frame. Sending between select rounds avoids aliasing the
/// client borrowed by the receive future.
enum Outbound {
    Request {
        req_id: RequestId,
        payload: FrontendRequest,
    },
    Reverse {
        req_id: RequestId,
        payload: ClientResponsePayload,
    },
    /// A JSON-RPC notification (no id, no reply). Used for client-to-server
    /// signals like session/cancel.
    Notification(AcpNotification),
}

impl Outbound {
    /// The request id this outbound carries, or None for a notification
    /// (which has no id and no reply).
    fn req_id(&self) -> Option<RequestId> {
        match self {
            Outbound::Request { req_id, .. } | Outbound::Reverse { req_id, .. } => Some(*req_id),
            Outbound::Notification(_) => None,
        }
    }
}

/// What the driver learned when it died: the cause and the request ids it
/// can prove were never attempted (the queue tail after a send failure).
/// Every other in-flight request is conservatively unknown — a write or
/// flush may have delivered the frame even though the carrier then broke.
pub(crate) struct DriverDeath {
    pub(crate) cause: String,
    pub(crate) not_sent: Vec<RequestId>,
}

impl DriverDeath {
    fn from_cause(cause: String) -> Self {
        Self {
            cause,
            not_sent: Vec::new(),
        }
    }
}

pub(crate) async fn drive_client(
    client: houyicoder_client::Client,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ClientCommand>,
    agent_tx: mpsc::Sender<SessionMessage>,
) {
    let death = drive_connection(client, cmd_rx, &agent_tx).await;
    // The command receiver dropped with the driver body's frame, so by the
    // time the App observes the event, every later send is refused
    // deterministically — no window exists where a command could buffer
    // into an orphaned channel and leave pane state waiting on a reply.
    if let Some(death) = death {
        let _send = agent_tx.send(SessionMessage::Connection(ConnectionEvent::Lost {
            cause: death.cause,
            not_sent: death.not_sent,
        }));
    }
}

/// The driver body: translate until the connection dies or the command
/// channel closes. Returns the death record for the caller to announce,
/// or None on a clean shutdown (no connection to lose).
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
async fn drive_connection(
    mut client: houyicoder_client::Client,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ClientCommand>,
    agent_tx: &mpsc::Sender<SessionMessage>,
) -> Option<DriverDeath> {
    if let Err(e) = client.connect().await {
        return Some(DriverDeath::from_cause(format!("connect failed: {e}")));
    }
    // Hello succeeded: announce readiness before any frame or request this
    // connection produces, so the App marks the connection Ready on a
    // confirmed handshake rather than inferring it from the object existing.
    let _send = agent_tx.send(SessionMessage::Connection(ConnectionEvent::Ready));
    let mut outbound: VecDeque<Outbound> = VecDeque::new();
    let mut unknown_reports = UnknownReportBudget::new();
    loop {
        while let Some(out) = outbound.pop_front() {
            let res = match out {
                Outbound::Request { req_id, payload } => client.send_request(req_id, payload).await,
                Outbound::Reverse { req_id, payload } => {
                    client.send_reverse_response(req_id, payload).await
                }
                Outbound::Notification(n) => client.send_notification(n).await,
            };
            if let Err(e) = res {
                // The failed frame is unknown (a write or flush may have
                // delivered it); the remaining queue tail is provably
                // not sent — the driver never attempted it.
                let not_sent = outbound.iter().filter_map(|o| o.req_id()).collect();
                return Some(DriverDeath {
                    cause: format!("send failed: {e}"),
                    not_sent,
                });
            }
        }
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(ClientCommand::SendMessage { req_id, session_id, content, disabled_skills }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::MessageSend { session_id, content, disabled_skills },
                    });
                }
                Some(ClientCommand::Verdict { req_id, decision }) => {
                    outbound.push_back(Outbound::Reverse {
                        req_id,
                        payload: ClientResponsePayload::Permission(decision),
                    });
                }
                Some(ClientCommand::TrustVerdict { req_id, accept }) => {
                    outbound.push_back(Outbound::Reverse {
                        req_id,
                        payload: ClientResponsePayload::TrustAccept(
                            TrustAccept { accepted: accept },
                        ),
                    });
                }
                Some(ClientCommand::StatusQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::Status,
                    });
                }
                Some(ClientCommand::TrajectoryQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::Trajectory,
                    });
                }
                Some(ClientCommand::ContextQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::Context,
                    });
                }
                Some(ClientCommand::CompactQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::Compact,
                    });
                }
                Some(ClientCommand::PermissionModeQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::PermissionMode,
                    });
                }
                Some(ClientCommand::PermissionRulesQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::PermissionRules,
                    });
                }
                Some(ClientCommand::ToolListQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::ToolList,
                    });
                }
                Some(ClientCommand::AgentsQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::Agents,
                    });
                }
                Some(ClientCommand::ChildTranscriptQuery { req_id, child_sid }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::ChildTranscript { child_sid },
                    });
                }
                Some(ClientCommand::HooksQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::Hooks,
                    });
                }
                Some(ClientCommand::SkillBodyQuery { req_id, name }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::SkillBody { name },
                    });
                }
                Some(ClientCommand::SkillsQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::Skills,
                    });
                }
                Some(ClientCommand::MemoryListQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::MemoryList,
                    });
                }
                Some(ClientCommand::MemoryShowQuery { req_id, key }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::MemoryShow { key },
                    });
                }
                Some(ClientCommand::MemoryToggleStateQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::MemoryToggleState,
                    });
                }
                Some(ClientCommand::MemoryToggleQuery { req_id, which }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::MemoryToggle { which },
                    });
                }
                Some(ClientCommand::MemoryForgetQuery { req_id, key, scope }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::MemoryForget { key, scope },
                    });
                }
                Some(ClientCommand::UndoQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::Undo,
                    });
                }
                Some(ClientCommand::ModelInfoQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::ModelInfo,
                    });
                }
                Some(ClientCommand::ModelSwitch {
                    req_id,
                    model,
                    effort,
                    effort_toggled,
                    speed,
                }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::ModelSet {
                            model,
                            effort,
                            effort_toggled,
                            speed,
                        },
                    });
                }
                Some(ClientCommand::RenameSessionQuery {
                    req_id,
                    session_id,
                    name,
                }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::RenameSession { session_id, name },
                    });
                }
                Some(ClientCommand::PermissionCycleModeQuery { req_id }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::PermissionCycleMode,
                    });
                }
                Some(ClientCommand::PermissionAddRuleQuery { req_id, rule }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::PermissionAddRule { rule },
                    });
                }
                Some(ClientCommand::PermissionRemoveRuleQuery { req_id, index }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::PermissionRemoveRule { index },
                    });
                }
                Some(ClientCommand::PermissionAddDirQuery { req_id, path }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::PermissionAddWorkingDir { path },
                    });
                }
                Some(ClientCommand::PermissionRemoveDirQuery { req_id, path }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::PermissionRemoveWorkingDir { path },
                    });
                }
                Some(ClientCommand::PermissionAskBeforeGitQuery { req_id, enabled }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::PermissionAskBeforeGit { enabled },
                    });
                }
                Some(ClientCommand::AbortRun { session_id }) => {
                    // A session/cancel notification: the server's mid-run
                    // select catches it and aborts the runner token. No id
                    // (no reply) — the run resolves Interrupted and the
                    // outcome returns on the original run's req_id.
                    let notif = AcpNotification::new(
                        "session/cancel",
                        serde_json::json!({ "sessionId": session_id.0 }),
                    );
                    outbound.push_back(Outbound::Notification(notif));
                }
                Some(ClientCommand::InjectUser { session_id, input }) => {
                    // A session/inject notification: the server enqueues the
                    // identified input for mid-turn injection. No reply; the
                    // message shows up in the transcript once the drive loop
                    // drains it, or runs as a follow-up if the run ends first.
                    outbound.push_back(Outbound::Notification(inject_notification(
                        &session_id,
                        &input,
                    )));
                }
                Some(ClientCommand::InjectToChild { child_sid, text }) => {
                    // Steering: route the text into a running child's inbox.
                    // The server's bus delivers it; the child drains at its
                    // next turn boundary. No reply; no parent turn starts.
                    outbound.push_back(Outbound::Notification(inject_child_notification(
                        &child_sid,
                        &text,
                    )));
                }
                Some(ClientCommand::CancelChildTurn { child_sid }) => {
                    // Per-turn abort (the teammate-view Esc path). The server
                    // cancels the viewed child's in-flight model fetch; the
                    // child's drive loop appends an interrupt marker + starts
                    // the next turn. No reply.
                    outbound.push_back(Outbound::Notification(
                        cancel_child_turn_notification(&child_sid),
                    ));
                }
                Some(ClientCommand::KillChild { child_sid }) => {
                    // Lifecycle kill of one child (the 'k' on a selected
                    // pill). The server cancels the child's lifecycle token
                    // so its drive loop returns terminal; the completion
                    // publishes + drops the pill row. No reply.
                    outbound.push_back(Outbound::Notification(
                        kill_child_notification(&child_sid),
                    ));
                }
                Some(ClientCommand::KillAllChildren) => {
                    // The fleet kill-all path ('K' two-press). The server
                    // kills every live background child; each completion
                    // publishes and drops its pill row. No reply.
                    outbound.push_back(Outbound::Notification(kill_all_notification()));
                }
                Some(ClientCommand::QueueRemove { session_id, id }) => {
                    // A session/queue_remove notification drops only the exact
                    // identified queue item. No reply.
                    outbound.push_back(Outbound::Notification(queue_remove_notification(
                        &session_id,
                        id,
                    )));
                }
                Some(ClientCommand::SessionReset {
                    req_id,
                    session_id,
                }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::SessionReset { session_id },
                    });
                }
                Some(ClientCommand::DebugSet { req_id, level }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::DebugSet { level },
                    });
                }
                None => return None,
            },
            frame = client.next_frame() => match frame {
                Ok(ServerFrame::Event(ev)) => match ev.payload {
                    FrontendEvent::SessionUpdate { update } => {
                        let _send = agent_tx
                            .send(SessionMessage::Event(ServerEvent::Frame(
                                TranscriptFrame::Session(update),
                            )));
                    }
                    FrontendEvent::Acpx { notification } => {
                        // Token-level deltas ride the acpx/llm/* stream as
                        // live preview; the authoritative AssistantMessage
                        // / Reasoning durable event replaces the accumulated
                        // preview when the turn lands, so deltas are NOT
                        // pushed as frames — they ship straight to the event
                        // loop and the transcript rebuild ignores them.
                        use houyicoder_protocol::acpx::AcpxMethod;
                        match notification.method {
                            AcpxMethod::LlmTextDelta => {
                                if let Some(text) = notification
                                    .params
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                {
                                    let _send = agent_tx.send(SessionMessage::Event(
                                        ServerEvent::Delta { text: text.to_string() },
                                    ));
                                }
                            }
                            AcpxMethod::LlmReasoningDelta => {
                                if let Some(text) = notification
                                    .params
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                {
                                    let _send = agent_tx.send(SessionMessage::Event(
                                        ServerEvent::ReasoningDelta {
                                            text: text.to_string(),
                                        },
                                    ));
                                }
                            }
                            AcpxMethod::ToolProgress => {
                                if let (Some(call_id), Some(elapsed)) = (
                                    notification.params.get("call_id").and_then(|v| v.as_str()),
                                    notification.params.get("elapsed_secs").and_then(|v| v.as_u64()),
                                ) {
                                    let lines = notification
                                        .params
                                        .get("lines")
                                        .and_then(|v| v.as_u64());
                                    let _send = agent_tx.send(SessionMessage::Event(
                                        ServerEvent::ToolProgress {
                                            call_id: call_id.to_string(),
                                            elapsed_secs: elapsed,
                                            lines,
                                        },
                                    ));
                                }
                            }
                            AcpxMethod::Unknown => {
                                // A method this build does not know: a newer
                                // session's extension, or a name this build
                                // writes wrong. Nothing here can draw it, and
                                // meeting one is not a failure — the read
                                // continues, and the diagnostic log records
                                // the payload keys.
                                if unknown_reports.grant() {
                                    let keys: Vec<&str> = notification
                                        .params
                                        .as_object()
                                        .map(|o| o.keys().map(String::as_str).collect())
                                        .unwrap_or_default();
                                    tracing::warn!("unknown acpx method dropped: {keys:?}");
                                }
                            }
                            _ => {
                                let _send = agent_tx.send(SessionMessage::Event(
                                    ServerEvent::Frame(TranscriptFrame::Acpx(notification)),
                                ));
                            }
                        }
                    }
                    FrontendEvent::QueuedInputCommitted { inputs } => {
                        let _send = agent_tx.send(SessionMessage::Event(
                            ServerEvent::QueuedInputCommitted { inputs },
                        ));
                    }
                    FrontendEvent::MemoryChanged {
                        id,
                        causality,
                        changes,
                        ..
                    } => {
                        let _send = agent_tx.send(SessionMessage::Event(ServerEvent::MemoryChanged {
                            id,
                            causality,
                            changes,
                        }));
                    }
                    FrontendEvent::SystemLine { text } => {
                        let _send = agent_tx.send(SessionMessage::Event(ServerEvent::SystemLine {
                            text,
                        }));
                    }
                    FrontendEvent::AgentStatus {
                        agent_id,
                        subagent_type,
                        turn,
                        tokens,
                        tool_uses,
                        last_activity,
                        completed,
                    } => {
                        let _send = agent_tx.send(SessionMessage::Event(ServerEvent::AgentStatus {
                            agent_id,
                            subagent_type,
                            turn,
                            tokens,
                            tool_uses,
                            last_activity,
                            completed,
                        }));
                    }
                    // A future event kind the driver does not model; ignore it
                    // rather than killing the driver.
                    _ => {}
                },
                Ok(ServerFrame::Request(ask)) => {
                    let req_id = ask.req_id;
                    if let ServerRequestPayload::Permission(p) = ask.payload {
                        // Every Frame up to this point has already shipped, so
                        // the event loop's own frame log is current and the
                        // transcript rebuild on receipt reads it directly.
                        let _send = agent_tx.send(SessionMessage::Request {
                            request: req_id,
                            payload: ServerRequest::Permission { ask: Box::new(p) },
                        });
                    } else if let ServerRequestPayload::TrustPrompt(t) = ask.payload {
                        let _send = agent_tx.send(SessionMessage::Request {
                            request: req_id,
                            payload: ServerRequest::Trust { prompt: t },
                        });
                    }
                }
                Ok(ServerFrame::Response(resp)) => {
                    let req_id = resp.req_id;
                    let response = match resp.payload {
                        ResponsePayload::RunOk(r) => Some(ServerResponse::Done { result: Ok(r) }),
                        ResponsePayload::RunErr(e) => {
                            Some(ServerResponse::Done { result: Err(e) })
                        }
                        ResponsePayload::Error(e) => {
                            // A protocol error is per-request, NOT a run
                            // completion (runs use RunOk/RunErr). The App
                            // routes by request id: a run's own error resolves
                            // its Done; a non-run verb's error becomes a
                            // system line (not a false run-end that would
                            // corrupt agent_busy mid-run).
                            Some(ServerResponse::Error { message: e.to_string() })
                        }
                        ResponsePayload::Status(s) => {
                            Some(ServerResponse::Status {
                                snapshot: Box::new(s),
                            })
                        }
                        ResponsePayload::Trajectory(resp) => Some(ServerResponse::Trajectory {
                            entries: resp.entries,
                            redundant: resp.redundant,
                        }),
                        ResponsePayload::Context(bd) => {
                            Some(ServerResponse::Context { breakdown: bd })
                        }
                        ResponsePayload::Compact(reply) => {
                            Some(ServerResponse::Compact { reply })
                        }
                        ResponsePayload::PermissionMode(mode) => {
                            Some(ServerResponse::PermissionMode { mode })
                        }
                        ResponsePayload::PermissionRules(rules) => {
                            Some(ServerResponse::PermissionRules { rules })
                        }
                        ResponsePayload::PermissionWorkingDirs(dirs) => {
                            Some(ServerResponse::PermissionDirs { dirs })
                        }
                        ResponsePayload::PermissionAskBeforeGit(enabled) => {
                            Some(ServerResponse::PermissionAskBeforeGit { enabled })
                        }
                        ResponsePayload::Debug(state) => Some(ServerResponse::Debug { state }),
                        ResponsePayload::Tools(tools) => Some(ServerResponse::Tools { tools }),
                        ResponsePayload::Agents(directory) => {
                            Some(ServerResponse::Agents { directory })
                        }
                        ResponsePayload::ChildTranscript { child_sid, frames } => {
                            // Convert the wire frames to the live-frame shape
                            // once, at the driver boundary. The fill site then
                            // runs transcript_from_frames unchanged.
                            Some(ServerResponse::ChildTranscript {
                                child_sid: child_sid.0,
                                frames: frames.into_iter().map(Into::into).collect(),
                            })
                        }
                        ResponsePayload::Hooks(hooks) => Some(ServerResponse::Hooks { hooks }),
                        ResponsePayload::Skills(skills) => {
                            Some(ServerResponse::Skills { skills })
                        }
                        ResponsePayload::SkillBody(body) => {
                            Some(ServerResponse::SkillBody { body })
                        }
                        ResponsePayload::MemoryList(entries) => {
                            Some(ServerResponse::MemoryList { entries })
                        }
                        ResponsePayload::MemoryShow(entry) => {
                            Some(ServerResponse::MemoryShow { entry })
                        }
                        ResponsePayload::ToggleState(state) => {
                            Some(ServerResponse::MemoryToggleState { state })
                        }
                        ResponsePayload::UndoResult(description) => {
                            Some(ServerResponse::Undo { description })
                        }
                        ResponsePayload::ModelResult(result) => {
                            Some(ServerResponse::Model { result })
                        }
                        ResponsePayload::ModelInfo(catalog) => {
                            Some(ServerResponse::ModelInfo { catalog })
                        }
                        // A known acknowledgement (e.g. SessionReset): the
                        // host already acted locally, but the reply keeps
                        // its request id like every other response.
                        ResponsePayload::Ack => Some(ServerResponse::Ack),
                        // A future payload shape the driver does not model
                        // carries nothing for the App; skip it rather than
                        // kill the driver.
                        _ => None,
                    };
                    if let Some(response) = response {
                        let _send =
                            agent_tx.send(SessionMessage::Response { request: req_id, response });
                    }
                }
                // A future server-frame shape the driver does not model; ignore
                // it rather than killing the driver.
                Ok(_) => {}
                Err(e) => {
                    // A read failure (the server closed or the transport
                    // failed) ends the driver, same as the connect and send
                    // exits. The announced death makes the App clear
                    // agent_busy and sweep pending pane state — without it
                    // the TUI waits on replies that can never arrive.
                    return Some(DriverDeath::from_cause(format!(
                        "connection lost: {e}"
                    )));
                }
            }
        }
    }
}

/// Build a session/inject notification with stable queue identity.
pub(crate) fn inject_notification(
    session_id: &FrontendSessionId,
    input: &QueuedInput,
) -> AcpNotification {
    AcpNotification::new(
        "session/inject",
        serde_json::json!({ "sessionId": session_id.0, "input": input }),
    )
}

/// Build a session/inject_child notification. Pure so the wire shape (the
/// childSid + text the server's handle_session_notification reads) is
/// unit-testable: a typo would make steering silently no-op.
pub(crate) fn inject_child_notification(child_sid: &str, text: &str) -> AcpNotification {
    AcpNotification::new(
        "session/inject_child",
        serde_json::json!({ "childSid": child_sid, "text": text }),
    )
}

/// Build a session/cancel_child_turn notification. Pure so the wire shape
/// (the childSid the server's handle_session_notification reads) is
/// unit-testable: a typo would make the abort silently no-op.
pub(crate) fn cancel_child_turn_notification(child_sid: &str) -> AcpNotification {
    AcpNotification::new(
        "session/cancel_child_turn",
        serde_json::json!({ "childSid": child_sid }),
    )
}

/// Build a session/queue_remove notification for one exact queue item.
pub(crate) fn queue_remove_notification(
    session_id: &FrontendSessionId,
    id: PendingInputId,
) -> AcpNotification {
    AcpNotification::new(
        "session/queue_remove",
        serde_json::json!({ "sessionId": session_id.0, "id": id }),
    )
}

/// Build a session/kill_all notification. Pure so the wire method name
/// matches what the server's handle_session_notification routes to
/// kill_all_children; a typo would make the kill silently no-op.
pub(crate) fn kill_all_notification() -> AcpNotification {
    AcpNotification::new("session/kill_all", serde_json::json!({}))
}

/// Build a session/kill_child notification. Pure so the method + childSid
/// match what the server's handle_session_notification routes to kill_child.
pub(crate) fn kill_child_notification(child_sid: &str) -> AcpNotification {
    AcpNotification::new(
        "session/kill_child",
        serde_json::json!({ "childSid": child_sid }),
    )
}

#[cfg(test)]
mod tests {
    use super::UnknownReportBudget;

    #[test]
    fn test_unknown_report_budget_spends() {
        // The bound's false branch has no other observable: the driver's
        // only report for a dropped notification is a diagnostic line.
        let mut budget = UnknownReportBudget::new();
        for _ in 0..UnknownReportBudget::LIMIT {
            assert!(budget.grant());
        }
        assert!(!budget.grant());
    }
}
