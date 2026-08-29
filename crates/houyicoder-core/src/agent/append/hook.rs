//! Hook-signal recording shared by the Runner's in-loop hook recorder and a
//! service-layer fire point. One recorder, two callers, no divergence.

use houyicoder_api::live::{LiveEvent, LiveSink};
use houyicoder_api::session::SessionLog;
use houyicoder_context::{HookVerdictKind, SessionId, TurnEventKind};

use super::super::hook::{
    HookEvent, HookVerdict,
    wire::{
        HookOutcome, verdict_on_hook_error, wire_error_kind, wire_error_reason, wire_event_kind,
        wire_verdict_kind,
    },
};
use super::super::obs_wire::SharedObservability;
use super::new_event;

/// Forward a system line to the live sink. No-op when no sink is wired.
/// Extracted from the Runner method so a service-layer fire point shares one
/// live-emit path with the in-loop hook recorder.
pub(crate) fn emit_live_line(live: Option<&LiveSink>, text: String) {
    if let Some(sink) = live {
        sink(&LiveEvent::SystemLine { text });
    }
}

/// Record one HookSignal per hook outcome to the session log, reading the
/// current turn/call coords from the shared observability log and emitting
/// user-visible lines (Observe notes, hook failures) through the live sink.
/// Extracted from Runner::append_hook_signals so a service-layer fire point
/// records with the same shape the Runner does. Best-effort: a store error is
/// dropped, not fatal (hook audit must not crash the run).
pub(crate) async fn record_hook_signals(
    store: &dyn SessionLog,
    obs: &SharedObservability,
    live: Option<&LiveSink>,
    session: SessionId,
    event: HookEvent,
    tool_name: Option<&str>,
    outcomes: &[HookOutcome],
) {
    let wire_event = wire_event_kind(event);
    let (turn, call_in_turn) = match obs.lock() {
        Ok(ol) => ol.turn_coords(),
        Err(_) => (0, 0),
    };
    for o in outcomes {
        let (verdict_kind, reason, triggered, error_kind) = match &o.result {
            Ok(HookVerdict::Allow) => continue,
            Ok(HookVerdict::Trigger(ev)) => (
                HookVerdictKind::Trigger,
                String::new(),
                Some(wire_event_kind(*ev)),
                None,
            ),
            Ok(HookVerdict::Deny(r)) => (HookVerdictKind::Deny, r.clone(), None, None),
            Ok(HookVerdict::Feedback(r)) => (HookVerdictKind::Feedback, r.clone(), None, None),
            Ok(HookVerdict::Observe(r)) => {
                emit_live_line(live, format!("hook {}: {r}", o.hook_name));
                (HookVerdictKind::Observe, r.clone(), None, None)
            }
            Ok(HookVerdict::Inject(r)) => (HookVerdictKind::Inject, r.clone(), None, None),
            Ok(HookVerdict::Ask(r)) => (HookVerdictKind::Ask, r.clone(), None, None),
            Err(e) => {
                emit_live_line(
                    live,
                    format!("hook {} failed: {}", o.hook_name, wire_error_reason(e)),
                );
                (
                    wire_verdict_kind(&verdict_on_hook_error()),
                    wire_error_reason(e),
                    None,
                    Some(wire_error_kind(e)),
                )
            }
        };
        let signal = TurnEventKind::HookSignal {
            event: wire_event,
            verdict: verdict_kind,
            error: error_kind,
            reason,
            hook_name: o.hook_name.clone(),
            tool_name: tool_name.map(str::to_string),
            triggered_event: triggered,
            turn: Some(turn),
            call_in_turn: Some(call_in_turn),
        };
        let _res = store.append(new_event(session, signal)).await;
    }
}
