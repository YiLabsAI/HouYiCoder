//! Reconnect replay, pending-turn recovery, and the per-connection session
//! server lifecycle.

use std::sync::Arc;

use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponsePayload, ServerFrame, ServerRequestEnvelope, ServerRequestPayload,
};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};

use crate::composition::SessionHost;
use crate::lifecycle::{LifecycleState, PendingPermission, PendingTurn};
use crate::protocol_adapter::{build_approval_request, parse_approval_decision};
use crate::server::approval::disclose_directory_grants;
use crate::server::{EventSequencer, FrameCarrier, Server};
use houyicoder_context::{
    EventId, PermissionVerdict, SessionDescriptorStore, SessionEvent, SessionLogEntry,
};

impl Server {
    /// Build a server re-hydrated from a session host. The runner and event
    /// sequencer retain their state across connection changes; the host keeps
    /// permission interruptions available for the next attachment.
    pub(crate) fn new_for_resume(
        runner: Arc<Runner>,
        session: SessionId,
        event_sequencer: EventSequencer,
        gate: Arc<dyn houyicoder_permission::ModeGate>,
        host: Arc<SessionHost>,
        append_notify: Arc<tokio::sync::Notify>,
    ) -> Self {
        let sandbox_session = runner.sandbox_session();
        Self {
            runner,
            session,
            event_sequencer,
            replay_after: None,
            next_req_id: 0,
            gate,
            sandbox_session,
            host: Some(host),
            settings_path: houyicoder_config::settings_path(),
            project_path: None,
            append_notify: Some(append_notify),
            descriptor_store: None,
            diagnostics: crate::diagnostics::handle(),
            // Resume path does not wire the bus yet; a reconnecting session
            // mid-child-approval is a follow-up (the parent serve path covers
            // the common case).
            bus: None,
        }
    }

    /// Share the store's Append Notify so the serve select drains durable
    /// events mid-run. The same Arc<Notify> is fed to the store
    /// impl at the composition root; the store fires notify_one per append
    /// and this select's notified() branch wakes to push the new event
    /// without waiting for the run future to resolve.
    pub fn with_append_notify(mut self, notify: Arc<tokio::sync::Notify>) -> Self {
        self.append_notify = Some(notify);
        self
    }

    /// Attach the descriptor store used by status and session updates.
    pub fn with_descriptor_store(
        mut self,
        descriptor_store: Arc<dyn SessionDescriptorStore>,
    ) -> Self {
        self.descriptor_store = Some(descriptor_store);
        self
    }

    /// Write the session sidecar's model field so a later --resume restores
    /// this session's model. A missing store means the session was built
    /// without sidecar persistence (the stub path), which is nothing to
    /// record rather than a failure; a store that rejects the write is
    /// reported so the transcript can say the switch will not survive resume.
    /// Takes its parts by value so the write can run on the blocking pool
    /// instead of stalling the serve loop.
    pub(super) fn write_sidecar_model(
        store: Option<Arc<dyn SessionDescriptorStore>>,
        session: SessionId,
        model: &str,
    ) -> Result<(), String> {
        let Some(store) = store else {
            return Ok(());
        };
        store
            .update_descriptor(session, &mut |descriptor| {
                descriptor.model = model.to_string();
            })
            .map(drop)
            .map_err(|e| e.to_string())
    }
}

/// Re-emit a parked PendingTurn a reattaching connection finds in the host
/// store, then resume the run. The turn carries the unanswered asks (re-sent
/// head first) plus the verdicts already received (fed to runner.resume
/// together with the new ones). If resume lands another Interruption, the
/// new asks are written as a fresh turn and re-emitted the same way. Returns
/// true when a turn was resumed (so serve can decide whether to expect a
/// MessageSend), false when no turn was parked. No RunOk is sent to the
/// reattaching client — it did not originate the MessageSend; the run's
/// continued events stream as ServerFrame::Event, and the outcome is
/// recorded server-side.
#[expect(clippy::too_many_lines, reason = "resume lifecycle")]
pub(crate) async fn resume_pending(
    server: &mut Server,
    io: &mut FrameCarrier,
) -> Result<bool, ProtocolError> {
    let Some(host) = server.host.clone() else {
        return Ok(false);
    };
    if host.store().pending(server.session).is_none() {
        return Ok(false);
    }
    loop {
        let mut turn = host
            .store()
            .pending(server.session)
            .expect("parked turn present");
        // Re-emit each remaining ask, read one verdict per ask, advance the
        // turn (pop remaining -> decided).
        let mut cancelled = false;
        'asks: while !turn.remaining.is_empty() {
            let ask_perm = turn.remaining.remove(0);
            let mut reason = server.reconstruct_reason(&ask_perm.tool, &ask_perm.input);
            let directory_grants =
                server.approval_directory_grants(&ask_perm.tool, &ask_perm.input);
            disclose_directory_grants(&mut reason, &directory_grants);
            let engine_approval = houyicoder_core::agent::ApprovalRequest::new(
                ask_perm.call_id.clone(),
                ask_perm.tool.clone(),
                ask_perm.input.clone(),
            );
            let ask_id = server.mint_req_id();
            let ask = ServerRequestEnvelope::new(
                ask_id,
                ServerRequestPayload::Permission(build_approval_request(
                    &engine_approval,
                    reason.as_ref(),
                    None,
                )),
            );
            server.send_typed(io, &ServerFrame::Request(ask)).await?;
            // Loop, not a single read: a reattaching connection's first status
            // tick, a racing poll, a session/cancel, or a mode-cycle can land
            // while the re-emitted ask is in flight. The prior single read +
            // fatal arm deadlocked (the reconnect TOCTOU variant). Same
            // paradigm as handle_approval: dispatch session/* + mode-cycle,
            // drop the rest, fatal only on client close.
            let decision = loop {
                let frame = match io.next_frame().await {
                    Some(f) => f,
                    None => {
                        return Err(ProtocolError::new(
                            ErrorCategory::Unavailable,
                            "client closed mid-re-emit",
                            false,
                        ));
                    }
                };
                if let Ok(notif) =
                    serde_json::from_str::<houyicoder_protocol::acp_wire::AcpNotification>(&frame)
                {
                    let is_cancel = notif.method == "session/cancel";
                    server.handle_session_notification(&notif);
                    if is_cancel {
                        // Esc mid-re-emit: abort + stop re-emitting the rest.
                        // Resume proceeds with the collected verdicts + the
                        // abort token surfaces the cancellation.
                        cancelled = true;
                        break houyicoder_protocol::frontend::run::ApprovalDecision {
                            call_id: ask_perm.call_id.clone(),
                            approved: false,
                            updated_input: None,
                            scope: "once".to_string(),
                        };
                    }
                    continue;
                }
                if let Ok(ClientFrame::Response(r)) = serde_json::from_str::<ClientFrame>(&frame)
                    && r.req_id == ask_id
                {
                    match r.payload {
                        ClientResponsePayload::Permission(d) => break d,
                        _ => {
                            return Err(ProtocolError::new(
                                ErrorCategory::InvalidFrame,
                                "expected a permission reverse response",
                                false,
                            ));
                        }
                    }
                }
                // Non-matching: drop, keep waiting. Surface it so a
                // protocol-incompatible client does not look stuck with no clue.
                tracing::warn!(
                    "re-emit ask-wait: dropped a non-matching frame (first 80 chars): {}",
                    frame.chars().take(80).collect::<String>()
                );
            };
            // Audit the verdict (keys on call_id — safe to append once; the
            // reattaching client answers what the prior connection never did).
            let verdict = if decision.approved {
                PermissionVerdict::Approved
            } else {
                PermissionVerdict::Denied
            };
            let audit = SessionLogEntry {
                id: EventId::new(),
                session: server.session,
                ts: super::now_millis(),
                prev_hash: None,
                event: SessionEvent::PermissionDecision {
                    call_id: ask_perm.call_id.clone(),
                    tool: ask_perm.tool.clone(),
                    verdict,
                    scope: decision.scope.clone(),
                },
            };
            if let Err(e) = server.runner.store().append(audit).await {
                tracing::warn!("permission-decision audit append failed: {e}");
            }
            if decision.approved
                && ask_perm.tool != houyicoder_protocol::extension::ENTITLEMENT_TOOL
            {
                server
                    .route_consent_with_grants(
                        &ask_perm.tool,
                        &ask_perm.input,
                        &decision.scope,
                        reason.as_ref(),
                        &directory_grants,
                    )
                    .map_err(|error| {
                        ProtocolError::new(
                            ErrorCategory::Unavailable,
                            format!(
                                "approved directory capability could not be installed: {error}"
                            ),
                            false,
                        )
                    })?;
            }
            host.store()
                .advance_pending(server.session, decision.clone());
            turn.decided.push(decision);
            if cancelled {
                // A cancel mid-re-emit aborts the run; stop re-emitting the
                // remaining asks so resume can surface the cancellation.
                break 'asks;
            }
        }
        // All remaining answered — resume with the full decided set.
        host.store().set_pending(server.session, None);
        let decisions: Vec<_> = turn
            .decided
            .iter()
            .map(|d| parse_approval_decision(d.clone()))
            .collect();
        let result = {
            let runner = Arc::clone(&server.runner);
            let resume_fut = runner.resume(server.session, &decisions);
            tokio::pin!(resume_fut);
            loop {
                let notify_fut = match &server.append_notify {
                    Some(n) => futures::future::Either::Left(n.notified()),
                    None => futures::future::Either::Right(futures::future::pending::<()>()),
                };
                let event_fut = server.event_sequencer.notified();
                tokio::select! {
                    r = &mut resume_fut => break r,
                    frame = io.next_frame() => match frame {
                        Some(f) => {
                            if let Ok(notif) = serde_json::from_str::<AcpNotification>(&f) {
                                server.handle_session_notification(&notif);
                            } else if let Ok(ClientFrame::Request(req)) =
                                serde_json::from_str::<ClientFrame>(&f)
                            {
                                server.handle_request_during_run(io, req).await;
                            }
                        }
                        None => {
                            return Err(ProtocolError::new(
                                ErrorCategory::Unavailable,
                                "client closed mid-resume",
                                false,
                            ));
                        }
                    },
                    _ = notify_fut => server.flush_events(io).await?,
                    _ = event_fut => server.flush_events(io).await?,
                }
            }
        };
        server.flush_events(io).await?;
        match result {
            Ok(run) => match run.outcome {
                houyicoder_core::agent::RunOutcome::Interruption(more) => {
                    // Resume produced more asks: write a fresh turn + loop.
                    let remaining = more
                        .iter()
                        .map(|a| PendingPermission {
                            call_id: a.call_id.clone(),
                            tool: a.tool_name.clone(),
                            input: a.input.clone(),
                        })
                        .collect::<Vec<_>>();
                    host.store().set_pending(
                        server.session,
                        Some(PendingTurn {
                            remaining,
                            decided: Vec::new(),
                        }),
                    );
                    continue;
                }
                // Final outcome (or error): the run is done. No RunOk to the
                // reattaching client (it did not originate the MessageSend);
                // the outcome is in the event log.
                _ => return Ok(true),
            },
            Err(_) => return Ok(true),
        }
    }
}

/// Drive one connection against a hosted session. The runner, reliable event
/// journal, sequence allocator, and durable cursor survive disconnect while
/// each attachment supplies a new carrier. A parked permission turn resumes
/// after reliable history replay.
///
/// Lease guard: a terminal session (Cancelled or Shutdown) refuses reattach —
/// the run was aborted or handed off, so there is nothing to re-emit. A
/// session already marked Running refuses a second concurrent serve — the
/// lease is held by a live connection (the single-writer-per-session
/// contract). Otherwise the serve takes the lease (marks Running) on entry
/// and releases it (marks Detached, retaining any parked turn) on exit.
pub(crate) async fn serve_session(
    host: Arc<SessionHost>,
    session: SessionId,
    io: FrameCarrier,
) -> Result<(), ProtocolError> {
    // Atomically check + take the lease under one lock. This closes the
    // TOCTOU race where two concurrent serve_session calls could both observe
    // Detached and both proceed to set Running.
    host.store().try_take_lease(session).map_err(|e| match e {
        crate::lifecycle::LifecycleError::LeaseHeld(holder) => ProtocolError::new(
            ErrorCategory::Unavailable,
            format!("session lease held by {holder}"),
            false,
        ),
        _ => ProtocolError::new(
            ErrorCategory::Unavailable,
            "session is terminal; no reattach",
            false,
        ),
    })?;
    let handle = host.clone_handle(session).ok_or_else(|| {
        ProtocolError::new(
            ErrorCategory::Unavailable,
            "no live runner for session",
            false,
        )
    })?;
    // try_take_lease already set Running under the lock; no separate set_state.
    let server = Server::new_for_resume(
        handle.runner,
        session,
        handle.event_sequencer,
        handle.gate,
        host.clone(),
        handle.append_notify,
    );
    let result = server.serve(io).await;
    // Release the lease: a clean exit or a disconnect both leave the session
    // detached (the parked PendingTurn, if any, is retained by the store for
    // the next reattaching connection). The full Shutdown-on-completion +
    // PendingPermission-during-park wiring is the cross-process cut; the
    // in-process host only needs Running-while-served vs Detached-between.
    host.store().set_state(session, LifecycleState::Detached);
    result
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "reconnect_tests.rs"]
mod reconnect_tests;

#[cfg(test)]
#[path = "orphan_recovery_tests.rs"]
mod orphan_recovery_tests;
