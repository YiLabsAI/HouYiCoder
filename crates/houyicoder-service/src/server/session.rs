//! The reconnect-replay entry points: a server re-hydrated from a session
//! host and the serve_session driver that rebuilds a Server per connection.
//! Kept in a child module so server.rs stays under the file-size gate;
//! inherent impls here access Server's private fields (a child module sees
//! the parent's privates).

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use houyicoder_context::SessionId;
use houyicoder_core::agent::Runner;
use houyicoder_protocol::acp_wire::AcpNotification;
use houyicoder_protocol::envelope::{
    ClientFrame, ClientResponsePayload, ServerFrame, ServerRequestEnvelope, ServerRequestPayload,
};
use houyicoder_protocol::frontend::run::ApprovalRequest;
use houyicoder_protocol::wire::{WireError, WireErrorKind};

use crate::composition::SessionHost;
use crate::lifecycle::{LifecycleState, PendingPermission, PendingTurn};
use crate::projection::parse_approval_decision;
use crate::server::{Server, ServerIo};
use houyicoder_context::{EventId, PermissionVerdict, TurnEvent, TurnEventKind};

impl Server {
    /// Build a server re-hydrated from a session host: the runner + the shared
    /// seq counter + the pushed-event cursor come from the host's live handle
    /// (so they survive a prior connection's disconnect), and the host
    /// reference is retained so the run path can write the parked PendingTurn
    /// and the disconnect paths can flush pushed_count back. serve_session is
    /// the only caller; the single-shot constructors leave host None.
    pub(crate) fn new_for_resume(
        runner: Arc<Runner>,
        session: SessionId,
        next_seq: Arc<AtomicU64>,
        pushed_count: usize,
        gate: Arc<dyn houyicoder_permission::ModeGate>,
        host: Arc<SessionHost>,
    ) -> Self {
        Self {
            runner,
            session,
            next_seq,
            next_req_id: 0,
            pushed_count,
            gate,
            sandbox_session: None,
            host: Some(host),
            settings_path: houyicoder_config::settings_path(),
            project_path: None,
            append_notify: None,
            meta_store: None,
            diagnostics: crate::diagnostics::handle(),
            // Resume path does not wire the bus yet; a reconnecting session
            // mid-child-approval is a follow-up (the parent serve path covers
            // the common case).
            bus: None,
        }
    }

    /// Share the store's Append Notify so the serve select drains durable
    /// events mid-run (route B). The same Arc<Notify> is fed to the store
    /// impl at the composition root; the store fires notify_one per append
    /// and this select's notified() branch wakes to push the new event
    /// without waiting for the run future to resolve.
    pub fn with_append_notify(mut self, notify: Arc<tokio::sync::Notify>) -> Self {
        self.append_notify = Some(notify);
        self
    }

    /// Attach the session-metadata sidecar store so the Status handler can
    /// project the identity fields (version / name / cwd / provenance) onto
    /// the wire snapshot. The composition root shares the same Arc it used
    /// to write the initial sidecar.
    pub fn with_meta_store(
        mut self,
        meta_store: Arc<dyn houyicoder_context::SessionMetaStore>,
    ) -> Self {
        self.meta_store = Some(meta_store);
        self
    }

    /// Write the session sidecar's model field so a later --resume restores
    /// this session's model. Best-effort: a missing store (stub path) or
    /// sidecar is skipped — the in-memory pick + settings.json persistence
    /// still take effect; only the resume-restore is lost.
    pub(super) fn persist_sidecar_model(&self, model: &str) {
        let Some(store) = self.meta_store.as_ref() else {
            return;
        };
        drop(store.update_meta(self.session, &mut |meta| meta.model = model.to_string()));
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
    io: &mut ServerIo,
) -> Result<bool, WireError> {
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
            let ask_id = server.mint_req_id();
            let ask = ServerRequestEnvelope::new(
                ask_id,
                ServerRequestPayload::Permission(ApprovalRequest {
                    call_id: ask_perm.call_id.clone(),
                    tool_name: ask_perm.tool.clone(),
                    input: ask_perm.input.clone(),
                    options: Vec::new(),
                    reason: None,
                    delegation: None,
                }),
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
                        server.flush_pushed_count();
                        return Err(WireError::new(
                            WireErrorKind::Unavailable,
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
                            return Err(WireError::new(
                                WireErrorKind::InvalidFrame,
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
            let audit = TurnEvent {
                id: EventId::new(),
                session: server.session,
                ts: super::now_millis(),
                prev_hash: None,
                kind: TurnEventKind::PermissionDecision {
                    call_id: ask_perm.call_id.clone(),
                    tool: ask_perm.tool.clone(),
                    verdict,
                    scope: decision.scope.clone(),
                },
            };
            if let Err(e) = server.runner.store().append(audit).await {
                tracing::warn!("permission-decision audit append failed: {e}");
            }
            if decision.approved {
                // Route by reason, not tool name: re-decide reconstructs the
                // Ask reason (same display-only reconstruction as
                // handle_approval), then route_consent sends a path-bounds ask
                // to the directory grant and everything else to the rule path.
                let reason = server.reconstruct_reason(&ask_perm.tool, &ask_perm.input);
                server.route_consent(
                    &ask_perm.tool,
                    &ask_perm.input,
                    &decision.scope,
                    reason.as_ref(),
                );
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
            let resume_fut = server.runner.resume(server.session, &decisions);
            tokio::pin!(resume_fut);
            loop {
                tokio::select! {
                    biased;
                    r = &mut resume_fut => break r,
                    frame = io.next_frame() => match frame {
                        Some(f) => {
                            if let Ok(notif) = serde_json::from_str::<AcpNotification>(&f)
                                && notif.method == "session/cancel"
                            {
                                server.runner.abort();
                            }
                        }
                        None => {
                            server.flush_pushed_count();
                            return Err(WireError::new(
                                WireErrorKind::Unavailable,
                                "client closed mid-resume",
                                false,
                            ));
                        }
                    },
                }
            }
        };
        // Push the trajectory events the resume produced (post-resume tool
        // results, the final assistant message, etc.) skipping the prefix the
        // prior connection already saw.
        let events = server.runner.store().trajectory_snapshot(server.session);
        for ev in events.iter().skip(server.pushed_count) {
            server.push_turn_event(io, ev).await?;
        }
        server.pushed_count = events.len();
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

/// Drive one connection against a session hosted by a SessionHost. Rebuilds a
/// Server from the host's live handle (so the Arc<Runner> + the shared seq
/// counter + the pushed-event cursor survive a prior connection's
/// disconnect), then runs serve. On disconnect the host retains everything —
/// a reattaching connection calls this again and resumes. serve calls
/// resume_pending after the handshake so a reattach re-emits a parked ask
/// before the client sends anything. Returns Unavailable when no live runner
/// is registered for the session (cross-process reconnect without a
/// checkpoint is the deferred Gap B).
/// Drive one connection against a session hosted by a SessionHost. Rebuilds a
/// Server from the host's live handle (so the Arc<Runner> + the shared seq
/// counter + the pushed-event cursor survive a prior connection's
/// disconnect), then runs serve. On disconnect the host retains everything —
/// a reattaching connection calls this again and resumes. serve calls
/// resume_pending after the handshake so a reattach re-emits a parked ask
/// before the client sends anything. Returns Unavailable when no live runner
/// is registered for the session (cross-process reconnect without a
/// checkpoint is the deferred Gap B).
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
    io: ServerIo,
) -> Result<(), WireError> {
    // Atomically check + take the lease under one lock. This closes the
    // TOCTOU race where two concurrent serve_session calls could both observe
    // Detached and both proceed to set Running.
    host.store().try_take_lease(session).map_err(|e| match e {
        crate::lifecycle::LifecycleError::LeaseHeld(holder) => WireError::new(
            WireErrorKind::Unavailable,
            format!("session lease held by {holder}"),
            false,
        ),
        _ => WireError::new(
            WireErrorKind::Unavailable,
            "session is terminal; no reattach",
            false,
        ),
    })?;
    let handle = host.clone_handle(session).ok_or_else(|| {
        WireError::new(
            WireErrorKind::Unavailable,
            "no live runner for session",
            false,
        )
    })?;
    // try_take_lease already set Running under the lock; no separate set_state.
    let server = Server::new_for_resume(
        handle.runner,
        session,
        handle.next_seq,
        handle.pushed_count,
        handle.gate,
        host.clone(),
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
#[path = "session_reconnect_tests.rs"]
mod reconnect_tests;

#[cfg(test)]
#[path = "session_orphan_reconnect_tests.rs"]
mod orphan_reconnect_tests;
