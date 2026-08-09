//! Drain-flow tests: the event loop's idle drain (continuous-state polling +
//! consumptive idempotency). A queued item auto-sends on a clean run end
//! (FinalOutput) — the user got their answer, drain FIFO. An interrupt/error
//! parks it for the user to pop to the input box via Esc + edit before
//! re-sending; a redirect on interrupt should not auto-fire the pending input.

#![cfg(test)]

use crate::pending_queue::PendingItem;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::run::{ContentBlock, RunOutcome, RunResult, StopReason};
use houyicoder_protocol::llm::Usage;

use crate::agent_message::AgentMessage;
use crate::composition;
use crate::state::{Screen, TranscriptLine};

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app
}

/// Done does not drain the queue: the drain moved to the event loop's idle
/// drain so a queued message auto-sends on any idle. Done only clears busy.
/// The queue stays intact after Done; the head leaves only when
/// drain_pending_head runs (via idle_drain).
#[test]
fn test_final_output_defers_drain() {
    let mut app = working();
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("head".into()));
    app.pending.push(PendingItem::Message("tail".into()));
    app.handle_agent_message(AgentMessage::Done {
        result: Ok(RunResult {
            outcome: RunOutcome::FinalOutput {
                content: vec![ContentBlock::Text { text: "ok".into() }],
            },
            usage: Usage::default(),
            turns: 1,
            stop_reason: StopReason::EndTurn,
        }),
    });
    assert!(!app.agent_busy, "busy cleared by run end");
    assert_eq!(
        app.pending,
        vec![
            PendingItem::Message("head".into()),
            PendingItem::Message("tail".into())
        ],
        "queue intact after Done (drain moved to idle_drain)"
    );
    // The idle drain: head leaves, tail stays.
    assert!(app.drain_pending_head(), "head drained by the idle drain");
    assert_eq!(app.pending, vec![PendingItem::Message("tail".into())]);
}

/// An Interrupted run clears busy + demotes the queued Message to
/// ParkedMessage (the server buffer is gone), then the idle drain auto-sends
/// it as the next turn. A queue processor that fires
/// the next item regardless of how the prior run ended.
#[test]
fn test_interrupt_demotes_then_drains() {
    let mut app = working();
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("parked".into()));
    app.handle_agent_message(AgentMessage::Done {
        result: Ok(RunResult {
            outcome: RunOutcome::Interrupted {
                reason: "user abort".into(),
            },
            usage: Usage::default(),
            turns: 0,
            stop_reason: StopReason::Cancelled,
        }),
    });
    assert!(!app.agent_busy, "busy cleared by interrupt");
    assert_eq!(
        app.pending,
        vec![PendingItem::ParkedMessage("parked".into())],
        "queued message demoted to ParkedMessage after an interrupt (server \
         buffer cleared; host FIFO preserved for the follow-up drain)"
    );
    // Auto-drain on any idle: the ParkedMessage sends as the next turn.
    assert!(app.drain_pending_head(), "parked message drains on idle");
    assert!(app.pending.is_empty(), "parked message consumed");
}

/// A RequestError whose req_id matches the in-flight run routes as a run
/// failure: handle_run_done clears busy. The drain is not gated on the
/// outcome, so any queued item auto-sends on the next idle.
#[test]
fn test_request_error_ends_run() {
    let mut app = working();
    app.agent_busy = true;
    app.active_run_req_id.set(Some(RequestId(7)));
    app.handle_agent_message(AgentMessage::RequestError {
        req_id: RequestId(7),
        message: "session mismatch".into(),
    });
    assert!(!app.agent_busy, "matching error clears busy");
    assert!(app.active_run_req_id.get().is_none(), "req_id cleared");
}

/// A RequestError whose req_id does NOT match the in-flight run is a non-run
/// error (e.g. a rejected status query): surfaced as a system line, busy
/// unchanged, no run-end.
#[test]
fn test_mismatched_error_system_line() {
    let mut app = working();
    app.agent_busy = true;
    app.active_run_req_id.set(Some(RequestId(7)));
    app.handle_agent_message(AgentMessage::RequestError {
        req_id: RequestId(99),
        message: "bad query".into(),
    });
    assert!(app.agent_busy, "non-matching error does not clear busy");
    assert!(
        app.active_run_req_id.get().is_some(),
        "active req_id untouched"
    );
}

/// drain_pending_head on an empty queue is a no-op (returns false) -- the
/// idle drain's consume action when there is nothing to consume.
#[test]
fn test_pending_head_empty_noop() {
    let mut app = working();
    assert!(!app.drain_pending_head(), "empty queue = no drain");
    assert!(app.pending.is_empty());
}

/// A state-changing command (/clear) submitted mid-run is deferred: enqueued
/// as a Command with a one-time "will clear" feedback, NOT executed now (the
/// transcript is not cleared mid-run). It drains + dispatches at idle.
#[test]
fn test_busy_clear_enqueues_feedback() {
    let mut app = working();
    app.agent_busy = true;
    app.transcript
        .push(TranscriptLine::User("pre-clear".into()));
    app.input.set("/clear".to_string());
    app.submit_input();
    assert_eq!(
        app.pending,
        vec![PendingItem::Command("/clear".into())],
        "/clear enqueued as a Command mid-run"
    );
    assert!(
        !app.transcript.is_empty(),
        "transcript NOT cleared mid-run (clear deferred)"
    );
}

/// A deferred Command drains + dispatches at idle. /clear enqueued mid-run
/// clears the session when the drain runs (strict FIFO head drain).
#[test]
fn test_drain_command_dispatches_clear() {
    let mut app = working();
    app.transcript.push(TranscriptLine::User("stale".into()));
    app.pending.push(PendingItem::Command("/clear".into()));
    assert!(app.drain_pending_head(), "Command drained");
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(_))),
        "clear dispatched (User lines cleared; the archive notice remains)"
    );
    assert!(app.pending.is_empty(), "Command consumed");
}

/// barrier_active is true when a Command sits ahead in the queue (any
/// position), false when only messages or nothing is queued. A pending
/// command will swap or reset the session, discarding the server-side
/// queue, so subsequent message enqueues must skip InjectUser.
#[test]
fn test_barrier_active_command_ahead() {
    let mut app = working();
    assert!(!app.barrier_active(), "empty queue = no barrier");
    app.pending.push(PendingItem::Message("task a".into()));
    assert!(!app.barrier_active(), "messages only = no barrier");
    app.pending
        .push(PendingItem::Command("/resume sid-b".into()));
    assert!(app.barrier_active(), "command ahead = barrier");
    // A message enqueued AFTER the command still sees the barrier.
    app.pending.push(PendingItem::Message("task c".into()));
    assert!(
        app.barrier_active(),
        "barrier holds for messages enqueued after the command"
    );
}

/// The barrier lifts once the command drains (is consumed): a message
/// enqueued after the command ran InjectUser normally. Uses /rewind (a
/// local stage command with no server effect) so the message keeps its
/// server copy -- /clear would orphan the server copy (see clear_orphans_pending_mirror).
/// Pins the "lifts on consume" contract so a future change does not make
/// the barrier sticky.
#[test]
fn test_barrier_lifts_command_drains() {
    let mut app = working();
    app.pending.push(PendingItem::Command("/rewind".into()));
    app.pending
        .push(PendingItem::Message("after rewind".into()));
    assert!(app.barrier_active(), "barrier before the command drains");
    // Drain the command. The message becomes head; barrier lifts.
    assert!(app.drain_pending_head(), "command drained");
    assert!(
        !app.barrier_active(),
        "barrier lifts once the command is consumed"
    );
}

/// /clear resets the server session, so a queued Message with a live server copy
/// is orphaned and must be demoted to ParkedMessage. With strict FIFO the
/// /clear Command sits at the head (ahead of the Message) so it drains first
/// and orphans the Message behind it. The host state invalidation runs even
/// when no req_id is minted (no client wired in the test harness) -- it is
/// decoupled from id-minting. Without this, a Message with a stale server copy
/// would let a newly enqueued message InjectUser past it (the barrier only
/// blocks on Command and ParkedMessage), leapfrogging.
#[test]
fn test_clear_orphans_pending_mirror() {
    let mut app = working();
    app.pending.push(PendingItem::Command("/clear".into()));
    app.pending
        .push(PendingItem::Message("queued after clear".into()));
    assert!(app.barrier_active(), "command ahead is a barrier");
    assert!(app.drain_pending_head(), "clear drained");
    assert!(
        app.pending
            .iter()
            .all(|it| matches!(it, PendingItem::ParkedMessage(_))),
        "a /clear orphans every queued Message to ParkedMessage"
    );
    assert!(
        app.barrier_active(),
        "a ParkedMessage head still blocks InjectUser (no copy to leapfrog)"
    );
}

/// Strict FIFO: a Message head drains before a Command behind it. The head
/// always goes first (no scan-past), so a Command waits its turn -- but it
/// never starves, because the head drains immediately on idle. Pins the
/// FIFO contract so a future change does not reintroduce head-of-line skip.
#[test]
fn test_head_drains_first_fifo() {
    let mut app = working();
    app.pending.push(PendingItem::Message("head msg".into()));
    app.pending.push(PendingItem::Command("/clear".into()));
    assert!(app.drain_pending_head(), "head Message drains");
    assert_eq!(
        app.pending,
        vec![PendingItem::Command("/clear".into())],
        "Command stays for the next drain (strict FIFO)"
    );
}

/// A ParkedMessage head drains the same way (spawn_run, no server copy): strict
/// FIFO holds regardless of whether the head carries a live server copy.
#[test]
fn test_parked_head_drains_first() {
    let mut app = working();
    app.pending
        .push(PendingItem::ParkedMessage("parked".into()));
    app.pending
        .push(PendingItem::Command("/resume sid-b".into()));
    assert!(app.drain_pending_head(), "parked head drains");
    assert_eq!(
        app.pending,
        vec![PendingItem::Command("/resume sid-b".into())],
        "Command stays for the next drain (strict FIFO, parked head)"
    );
}
