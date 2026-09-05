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
    app.pending.push(PendingItem::ParkedMessage("tail".into()));
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
            PendingItem::ParkedMessage("tail".into())
        ],
        "queue intact after Done (drain moved to idle_drain)"
    );
    // The idle drain: head leaves, tail stays parked (no live copy while idle).
    assert!(app.drain_pending_head(), "head drained by the idle drain");
    assert_eq!(app.pending, vec![PendingItem::ParkedMessage("tail".into())]);
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

/// Single-copy invariant: a second message enqueued while busy parks (no
/// server copy), so at most one item holds a live copy. The head holds it
/// (Message); every item past it parks. An Esc recall then races a single
/// copy, and the strip's "-> next" marks the sole live item.
#[test]
fn test_second_enqueue_parks() {
    let mut app = working();
    app.agent_busy = true;
    app.spawn_run("task a".into());
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("task a".into())],
        "empty queue gives the head the copy"
    );
    app.spawn_run("task b".into());
    assert_eq!(
        app.pending,
        vec![
            PendingItem::Message("task a".into()),
            PendingItem::ParkedMessage("task b".into()),
        ],
        "non-empty queue parks the newcomer; one live copy"
    );
    assert_eq!(
        app.pending
            .iter()
            .filter(|it| matches!(it, PendingItem::Message(_)))
            .count(),
        1,
        "exactly one live item"
    );
}

/// promote_next_pending contract: a Command head is a barrier -- it does not
/// promote, and nothing past it promotes either, because promoting past a
/// state-changing command would let the model consume a message before the
/// command resets or swaps the session. So with a Command at the head and a
/// ParkedMessage behind it, promote_next leaves both untouched.
#[test]
fn test_command_head_blocks() {
    let mut app = working();
    app.agent_busy = true;
    app.pending.push(PendingItem::Command("/rewind".into()));
    app.pending.push(PendingItem::ParkedMessage("after".into()));
    app.promote_next_pending();
    assert_eq!(
        app.pending[0],
        PendingItem::Command("/rewind".into()),
        "Command head is a barrier; promote_next leaves it"
    );
    assert_eq!(
        app.pending[1],
        PendingItem::ParkedMessage("after".into()),
        "promote_next does not promote past a Command head"
    );
}

/// promote_next_pending contract: once the Command barrier drains (removed
/// from the head), the next ParkedMessage promotes into the live-copy
/// slot -- the queue re-promotes the next head once the state-changing
/// command has cleared the way.
#[test]
fn test_command_drain_promotes() {
    let mut app = working();
    app.agent_busy = true;
    app.pending.push(PendingItem::Command("/rewind".into()));
    app.pending.push(PendingItem::ParkedMessage("after".into()));
    // The Command drains (local dispatch removes it from the head).
    app.pending.remove(0);
    app.promote_next_pending();
    assert_eq!(
        app.pending[0],
        PendingItem::Message("after".into()),
        "after the Command barrier drains, promote_next promotes the next head"
    );
}

/// An orphaned message (demoted to Parked by a prior interrupt) is promoted
/// into the next run when a subsequent enqueue fires promote_next_pending.
/// This is intended: the strip shows the orphan as queued (it will auto-run),
/// and FIFO places it ahead of the newcomer. The orphan gets the single live
/// copy; the newcomer parks. Without this test, a future change that gates
/// promote on last_run_final (stranding orphans until manual recall) would
/// silently pass -- pinning the behavior makes the design choice explicit.
#[test]
fn test_orphan_promoted_on_enqueue() {
    let mut app = working();
    // Simulate a prior interrupt that orphaned a queued message.
    app.pending
        .push(PendingItem::ParkedMessage("orphan".into()));
    app.status.last_run_final = false;
    // User submits a new message -> real-spawn path (agent_busy was false).
    // The test harness has no session, so spawn_run returns early without
    // setting agent_busy; simulate the post-spawn state manually.
    app.agent_busy = true;
    assert_eq!(
        app.pending[0],
        PendingItem::ParkedMessage("orphan".into()),
        "real-spawn path does not promote; orphan stays parked"
    );
    // User submits another message while the new run is busy -> queue path
    // fires promote_next_pending, which promotes the orphaned head.
    app.spawn_run("another".into());
    assert_eq!(
        app.pending[0],
        PendingItem::Message("orphan".into()),
        "enqueue-time promote fires for the orphaned head (FIFO: it runs first)"
    );
    assert_eq!(
        app.pending[1],
        PendingItem::ParkedMessage("another".into()),
        "the newcomer parks behind the promoted orphan"
    );
    assert_eq!(
        app.pending
            .iter()
            .filter(|it| matches!(it, PendingItem::Message(_)))
            .count(),
        1,
        "exactly one live copy (the orphan); the newcomer has none"
    );
}

/// /clear resets the server session, so a queued Message with a live server copy
/// is orphaned and must be demoted to ParkedMessage. With strict FIFO the
/// /clear Command sits at the head (ahead of the Message) so it drains first
/// and orphans the Message behind it. The host state invalidation runs even
/// when no req_id is minted (no client wired in the test harness) -- it is
/// decoupled from id-minting. Without demotion the single-copy invariant
/// breaks: a stale-live item the run no longer backs would strand.
#[test]
fn test_clear_orphans_pending_mirror() {
    let mut app = working();
    app.pending.push(PendingItem::Command("/clear".into()));
    app.pending
        .push(PendingItem::Message("queued after clear".into()));
    assert!(!app.pending.is_empty(), "command ahead blocks promoting");
    assert!(app.drain_pending_head(), "clear drained");
    assert!(
        app.pending
            .iter()
            .all(|it| matches!(it, PendingItem::ParkedMessage(_))),
        "a /clear orphans every queued Message to ParkedMessage"
    );
    assert!(
        app.pending
            .iter()
            .all(|it| !matches!(it, PendingItem::Message(_))),
        "no item holds a live server copy after /clear orphans the queue"
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
