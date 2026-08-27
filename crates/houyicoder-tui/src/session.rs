//! The live session between the TUI and the engine. The TUI holds one
//! Session per wired backend; the session owns the outbound command channel,
//! the inbound agent-message receiver, the monotonic request-id counter, and
//! the background driver task that pumps the protocol client.
//!
//! The driver (drive_client) is the stateless wire translator: it reads
//! inbound server frames and translates them to AgentMessage values on the
//! inbound channel, and it drains outbound ClientCommand values and ships
//! them as wire requests. The session history (the accumulated frames) and
//! the transcript projection live on App, not here — this module moves bytes
//! across the boundary, nothing more. The layering is the same in kind:
//! the SDK yields message deltas and the frontend owns the message list.
//!
//! The driver runs on the shared tokio runtime the composition root also uses
//! for the server task; ratatui's render loop is blocking and synchronous, so
//! it cannot await the client's async frame stream directly. The driver task
//! is the TUI's equivalent of awaiting an SDK generator inside the render
//! cycle — the runtime is already paid for, the task parks idle between
//! frames, and the select multiplexes inbound/outbound without the
//! recv-future's exclusive client borrow aliasing on a send.

use std::collections::VecDeque;
use std::sync::mpsc;

use houyicoder_protocol::envelope::{
    ClientResponsePayload, RequestId, ResponsePayload, ServerFrame, ServerRequestPayload,
};
use houyicoder_protocol::frontend::FrontendEventKind;
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::frontend::SessionId as WireSessionId;
use houyicoder_protocol::frontend::run::RunError;

use crate::agent_message::{AgentMessage, ClientCommand};
use crate::transcript::TranscriptFrame;

/// An outbound frame the driver should send on the wire. Held in a queue the
/// driver drains between select rounds so the select branches never borrow the
/// client (the recv future borrows the client exclusively for its whole life;
/// sending from inside a branch would alias it).
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
    Notification(houyicoder_protocol::acp_wire::AcpNotification),
}

/// The live session with the engine. Owns the command channel (App to driver),
/// the message channel (driver to App), the request-id counter, and the driver
/// task handle. Drop closes the channels and detaches the driver.
pub struct Session {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<ClientCommand>,
    agent_rx: mpsc::Receiver<AgentMessage>,
    next_req_id: std::cell::Cell<u64>,
    _driver: tokio::task::JoinHandle<()>,
}

impl Session {
    /// Spawn the driver on the shared runtime and return a Session holding the
    /// two ends App uses (the command sender + the message receiver) plus the
    /// request-id counter. The driver takes ownership of the client; App holds
    /// only this handle.
    pub fn spawn(
        client: houyicoder_client::Client,
        agent_tx: mpsc::Sender<AgentMessage>,
        agent_rx: mpsc::Receiver<AgentMessage>,
        runtime: &tokio::runtime::Runtime,
    ) -> Self {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ClientCommand>();
        let _driver = runtime.spawn(drive_client(client, cmd_rx, agent_tx));
        Self {
            cmd_tx,
            agent_rx,
            next_req_id: std::cell::Cell::new(0),
            _driver,
        }
    }

    /// Mint a fresh request id. Monotonic within the session; the matching
    /// response returns it (the driver pairs by the run boundary, not by id,
    /// since only one run is live at a time).
    pub fn mint_request_id(&self) -> RequestId {
        let id = self.next_req_id.get();
        self.next_req_id.set(id.wrapping_add(1));
        RequestId(id)
    }

    /// Ship a command to the driver (fire-and-forget). The driver drains and
    /// translates it to an outbound wire request on its next select round.
    pub fn send(&self, cmd: ClientCommand) {
        let _send = self.cmd_tx.send(cmd);
    }

    /// Drain one inbound agent message, if any is pending. Non-blocking; the
    /// render loop calls this each tick. Returns None when the channel is empty
    /// or the driver has detached.
    pub fn poll(&mut self) -> Option<AgentMessage> {
        self.agent_rx.try_recv().ok()
    }

    /// Ship a status query to refresh the cached status snapshot. Used by the
    /// status command and the periodic idle poll.
    pub fn request_status(&self) {
        let req_id = self.mint_request_id();
        self.send(ClientCommand::StatusQuery { req_id });
    }

    /// Ship a rename request so the server persists the session name to the
    /// sidecar. The reply is a StatusSnapshot routed to StatusResult, so the
    /// pane + the terminal tab title refresh together. The session_id is the
    /// App's current session (a rename only applies to the live session).
    pub fn request_rename(&self, session_id: WireSessionId, name: String) {
        let req_id = self.mint_request_id();
        self.send(ClientCommand::RenameSessionQuery {
            req_id,
            session_id,
            name,
        });
    }

    /// Ship a permission-mode query so the server reports the current
    /// permission mode, seeding the mode cache for the status-bar pill. Used
    /// once on the first idle poll to fill the initial gap; later mode cycles
    /// update the cache directly.
    pub fn request_permission_mode(&self) {
        let req_id = self.mint_request_id();
        self.send(ClientCommand::PermissionModeQuery { req_id });
    }
}

/// The background driver: own the protocol client, translate inbound server
/// frames to AgentMessage values on agent_tx, and drain outbound
/// ClientCommand values from cmd_rx into wire requests. The driver is
/// stateless: each durable frame ships as an AgentMessage::Frame so the event
/// loop owns the history. Deltas ride the acpx/llm/* stream as live preview
/// (Delta/ReasoningDelta), not accumulated. Outbound sends queue between
/// select rounds so neither branch borrows the client (the recv future holds
/// the client exclusively for its whole life).
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
async fn drive_client(
    mut client: houyicoder_client::Client,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<ClientCommand>,
    agent_tx: mpsc::Sender<AgentMessage>,
) {
    if let Err(e) = client.connect().await {
        let _send = agent_tx.send(AgentMessage::Done {
            result: Err(RunError {
                kind: "wire".to_string(),
                message: format!("connect failed: {e}"),
            }),
        });
        return;
    }
    let mut outbound: VecDeque<Outbound> = VecDeque::new();
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
                let _send = agent_tx.send(AgentMessage::Done {
                    result: Err(RunError {
                        kind: "wire".to_string(),
                        message: format!("send failed: {e}"),
                    }),
                });
                return;
            }
        }
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(ClientCommand::SendMessage { req_id, session_id, content }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::MessageSend { session_id, content },
                    });
                }
                Some(ClientCommand::Verdict { req_id, decision }) => {
                    outbound.push_back(Outbound::Reverse {
                        req_id,
                        payload: ClientResponsePayload::Permission(decision),
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
                }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::ModelSet {
                            model,
                            effort,
                            effort_toggled,
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
                Some(ClientCommand::PermissionAddWorkingDirQuery { req_id, path }) => {
                    outbound.push_back(Outbound::Request {
                        req_id,
                        payload: FrontendRequest::PermissionAddWorkingDir { path },
                    });
                }
                Some(ClientCommand::PermissionRemoveWorkingDirQuery { req_id, path }) => {
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
                    let notif = houyicoder_protocol::acp_wire::AcpNotification::new(
                        "session/cancel",
                        serde_json::json!({ "sessionId": session_id.0 }),
                    );
                    outbound.push_back(Outbound::Notification(notif));
                }
                Some(ClientCommand::InjectUser { session_id, text }) => {
                    // A session/inject notification: the server enqueues the
                    // text on the runner for mid-turn injection. No reply;
                    // the message shows up in the transcript once the drive
                    // loop drains it at the next turn boundary, or runs as a
                    // follow-up if the run ends first.
                    outbound.push_back(Outbound::Notification(inject_notification(
                        &session_id,
                        &text,
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
                Some(ClientCommand::QueueRemove { session_id, text }) => {
                    // A session/queue_remove notification: the server drops
                    // the first queued message whose text matches. No reply.
                    outbound.push_back(Outbound::Notification(queue_remove_notification(
                        &session_id,
                        &text,
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
                None => return,
            },
            frame = client.next_frame() => match frame {
                Ok(ServerFrame::Event(ev)) => match ev.payload {
                    FrontendEventKind::SessionUpdate { update } => {
                        let _send = agent_tx
                            .send(AgentMessage::Frame(TranscriptFrame::Session(update)));
                    }
                    FrontendEventKind::Acpx { notification } => {
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
                                    let _send = agent_tx
                                        .send(AgentMessage::Delta { text: text.to_string() });
                                }
                            }
                            AcpxMethod::LlmReasoningDelta => {
                                if let Some(text) = notification
                                    .params
                                    .get("text")
                                    .and_then(|v| v.as_str())
                                {
                                    let _send = agent_tx.send(AgentMessage::ReasoningDelta {
                                        text: text.to_string(),
                                    });
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
                                    let _send = agent_tx.send(AgentMessage::ToolProgress {
                                        call_id: call_id.to_string(),
                                        elapsed_secs: elapsed,
                                        lines,
                                    });
                                }
                            }
                            _ => {
                                let _send = agent_tx
                                    .send(AgentMessage::Frame(TranscriptFrame::Acpx(notification)));
                            }
                        }
                    }
                    FrontendEventKind::QueueConsumed { texts } => {
                        let _send = agent_tx.send(AgentMessage::QueueConsumed { texts });
                    }
                    FrontendEventKind::MemorySaved { count, kind } => {
                        let _send = agent_tx.send(AgentMessage::MemorySaved {
                            count,
                            kind,
                        });
                    }
                    FrontendEventKind::SystemLine { text } => {
                        let _send = agent_tx.send(AgentMessage::SystemLine { text });
                    }
                    FrontendEventKind::AgentStatus {
                        agent_id,
                        subagent_type,
                        turn,
                        tokens,
                        tool_uses,
                        last_activity,
                        completed,
                    } => {
                        let _send = agent_tx.send(AgentMessage::AgentStatus {
                            agent_id,
                            subagent_type,
                            turn,
                            tokens,
                            tool_uses,
                            last_activity,
                            completed,
                        });
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
                        let _send = agent_tx.send(AgentMessage::PermissionAsk {
                            req_id,
                            ask: p,
                        });
                    }
                }
                Ok(ServerFrame::Response(resp)) => match resp.payload {
                    ResponsePayload::RunOk(r) => {
                        let _send = agent_tx.send(AgentMessage::Done { result: Ok(r) });
                    }
                    ResponsePayload::RunErr(e) => {
                        let _send = agent_tx.send(AgentMessage::Done { result: Err(e) });
                    }
                    ResponsePayload::Error(e) => {
                        // A wire error is per-request, NOT a run completion
                        // (runs use RunOk/RunErr). Carry the req_id so the App
                        // routes: a run's own error -> Done{Err}; a non-run
                        // verb's error -> a system line (not a false run-end
                        // that would corrupt agent_busy mid-run).
                        let req_id = resp.req_id;
                        let _send = agent_tx.send(AgentMessage::RequestError {
                            req_id,
                            message: e.to_string(),
                        });
                    }
                    ResponsePayload::Ack => {}
                    ResponsePayload::Status(s) => {
                        let _send = agent_tx.send(AgentMessage::StatusResult { snapshot: s });
                    }
                    ResponsePayload::Trajectory(resp) => {
                        let _send = agent_tx.send(AgentMessage::TrajectoryResult {
                            entries: resp.entries,
                            redundant: resp.redundant,
                        });
                    }
                    ResponsePayload::Context(bd) => {
                        let _send = agent_tx.send(AgentMessage::ContextResult { breakdown: bd });
                    }
                    ResponsePayload::Compact(reply) => {
                        let _send = agent_tx.send(AgentMessage::CompactResult { reply });
                    }
                    ResponsePayload::PermissionMode(mode) => {
                        let _send = agent_tx.send(AgentMessage::PermissionModeResult { mode });
                    }
                    ResponsePayload::PermissionRules(rules) => {
                        let _send = agent_tx.send(AgentMessage::PermissionRulesResult { rules });
                    }
                    ResponsePayload::PermissionWorkingDirs(dirs) => {
                        let _send =
                            agent_tx.send(AgentMessage::PermissionWorkingDirsResult { dirs });
                    }
                    ResponsePayload::PermissionAskBeforeGit(enabled) => {
                        let _send =
                            agent_tx.send(AgentMessage::PermissionAskBeforeGitResult { enabled });
                    }
                    ResponsePayload::Debug(state) => {
                        let _send = agent_tx.send(AgentMessage::DebugResult { state });
                    }
                    ResponsePayload::Tools(tools) => {
                        let _send = agent_tx.send(AgentMessage::ToolListResult { tools });
                    }
                    ResponsePayload::Agents(directory) => {
                        let _send = agent_tx.send(AgentMessage::AgentsResult { directory });
                    }
                    ResponsePayload::ChildTranscript { child_sid, frames } => {
                        // Convert the wire frames to the live-frame shape once,
                        // at the driver boundary. The fill site then runs
                        // transcript_from_frames unchanged.
                        let frames: Vec<crate::transcript::TranscriptFrame> =
                            frames.into_iter().map(Into::into).collect();
                        let _send = agent_tx.send(AgentMessage::ChildTranscriptResult {
                            child_sid: child_sid.0,
                            frames,
                        });
                    }
                    ResponsePayload::Hooks(hooks) => {
                        let _send = agent_tx.send(AgentMessage::HooksResult { hooks });
                    }
                    ResponsePayload::MemoryList(entries) => {
                        let _send = agent_tx.send(AgentMessage::MemoryListResult { entries });
                    }
                    ResponsePayload::MemoryShow(entry) => {
                        let _send = agent_tx.send(AgentMessage::MemoryShowResult { entry });
                    }
                    ResponsePayload::ToggleState(state) => {
                        let _send =
                            agent_tx.send(AgentMessage::MemoryToggleStateResult { state });
                    }
                    ResponsePayload::UndoResult(description) => {
                        let _send =
                            agent_tx.send(AgentMessage::UndoResult { description });
                    }
                    ResponsePayload::ModelResult(applied) => {
                        let _send = agent_tx.send(AgentMessage::ModelResult {
                            model: applied.model,
                            effort: applied.effort,
                        });
                    }
                    ResponsePayload::ModelInfo(catalog) => {
                        let _send = agent_tx.send(AgentMessage::ModelInfoResult { catalog });
                    }
                    _ => {}
                },
                // A future server-frame shape the driver does not model; ignore
                // it rather than killing the driver.
                Ok(_) => {}
                Err(e) => {
                    // A read failure (the server closed or a wire error) ends
                    // the driver. Surface it as Done{Err} so the App clears
                    // agent_busy — the two other drive_client error exits
                    // (connect, send) already send Done{Err}; without it the
                    // TUI waits on a run that can never finish.
                    let _send = agent_tx.send(AgentMessage::Done {
                        result: Err(RunError {
                            kind: "wire".to_string(),
                            message: format!("connection lost: {e}"),
                        }),
                    });
                    return;
                }
            }
        }
    }
}

/// Build a session/inject notification. Pure so the wire shape (method +
/// params the server's handle_session_notification reads) is unit-testable:
/// a typo here would make mid-turn injection silently no-op (the server
/// would not match the method or find the text param).
fn inject_notification(
    session_id: &houyicoder_protocol::frontend::SessionId,
    text: &str,
) -> houyicoder_protocol::acp_wire::AcpNotification {
    houyicoder_protocol::acp_wire::AcpNotification::new(
        "session/inject",
        serde_json::json!({ "sessionId": session_id.0, "text": text }),
    )
}

/// Build a session/inject_child notification. Pure so the wire shape (the
/// childSid + text the server's handle_session_notification reads) is
/// unit-testable: a typo would make steering silently no-op.
fn inject_child_notification(
    child_sid: &str,
    text: &str,
) -> houyicoder_protocol::acp_wire::AcpNotification {
    houyicoder_protocol::acp_wire::AcpNotification::new(
        "session/inject_child",
        serde_json::json!({ "childSid": child_sid, "text": text }),
    )
}

/// Build a session/queue_remove notification. Pure for the same reason:
/// the wire shape must match what the server reads to drop a queued message.
fn queue_remove_notification(
    session_id: &houyicoder_protocol::frontend::SessionId,
    text: &str,
) -> houyicoder_protocol::acp_wire::AcpNotification {
    houyicoder_protocol::acp_wire::AcpNotification::new(
        "session/queue_remove",
        serde_json::json!({ "sessionId": session_id.0, "text": text }),
    )
}

#[cfg(test)]
#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
