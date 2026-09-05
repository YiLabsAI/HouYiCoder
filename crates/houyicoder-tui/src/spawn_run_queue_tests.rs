//! Queue-path invariants for spawn_run. Split out of run_control_tests so
//! that file stays under the size gate; this file holds the regression tests
//! for the spawn_run queue path (a second Enter while agent_busy).

use super::run_control_tests::app_with_provider;
use crate::agent_message::AgentMessage;
use crate::pending_queue::PendingItem;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_provider::FakeProvider;
use std::sync::Arc;

/// Regression: spawn_run on the queue path (agent_busy) must NOT overwrite
/// active_run_req_id. The in-flight run's id must stay set so a wire Error
/// for IT routes as a run failure (Done{Err}); minting + setting a fresh id
/// here (one that never ships) would mis-route the in-flight Error to a
/// system line, stranding agent_busy + never draining the queue — the very
/// corruption the req_id routing fix guards against, re-introduced by the
/// queue path.
#[test]
fn test_busy_queue_keeps_reqid() {
    let p = Arc::new(FakeProvider::text("ok"));
    let mut app = app_with_provider(p, ToolRegistry::new());
    // Simulate an in-flight run: busy + its req_id tracked.
    app.agent_busy = true;
    let in_flight = houyicoder_protocol::envelope::RequestId(42);
    app.active_run_req_id.set(Some(in_flight));
    // A second Enter while busy takes the queue path.
    app.spawn_run("second".into());
    assert_eq!(
        app.active_run_req_id.get(),
        Some(in_flight),
        "queue path must not overwrite the in-flight run's req_id"
    );
    assert_eq!(app.pending.len(), 1, "second input queued");
    assert_eq!(app.pending[0], PendingItem::Message("second".into()));
}

/// Single-copy invariant on enqueue: a message enqueued while busy parks
/// whenever the queue is non-empty, regardless of the head's type. A
/// ParkedMessage head (carried across a swap, or orphaned by an interrupt
/// or /clear) gets promoted (promote_next promotes it), and the newcomer
/// parks behind it; a Message head already holds the copy, and the
/// newcomer parks too. Either way at most one live copy exists, so a
/// mid-turn QueueConsumed cannot leapfrog a parked item (FIFO across the
/// host/server split).
#[test]
fn test_parked_head_promotes() {
    let p = Arc::new(FakeProvider::text("ok"));
    let mut app = app_with_provider(p, ToolRegistry::new());
    app.agent_busy = true;
    // A parked head (e.g. carried across a swap, demoted because the old
    // server queue was empty). promote_next re-promotes it in the current run.
    app.pending
        .push(PendingItem::ParkedMessage("carried".into()));
    app.spawn_run("newcomer".into());
    assert_eq!(
        app.pending[0],
        PendingItem::Message("carried".into()),
        "the parked head is promoted when a run is in flight"
    );
    assert_eq!(
        app.pending[1],
        PendingItem::ParkedMessage("newcomer".into()),
        "the newcomer parks behind the live head; one live copy"
    );
    // A Message head already holds the copy; the newcomer still parks.
    let mut app2 = app_with_provider(Arc::new(FakeProvider::text("ok")), ToolRegistry::new());
    app2.agent_busy = true;
    app2.pending.push(PendingItem::Message("injected".into()));
    app2.spawn_run("newcomer".into());
    assert_eq!(
        app2.pending[1],
        PendingItem::ParkedMessage("newcomer".into()),
        "a Message head holds the copy; the newcomer parks so only one races"
    );
}

/// Single-copy chain on consume: when the drive_loop drains the live head,
/// QueueConsumed removes it + promote_next promotes the next parked head
/// into the live-copy slot (Message + InjectUser). So the run keeps
/// draining the queue one turn boundary at a time, with exactly one live
/// copy at any instant. Red without promote_next in the QueueConsumed
/// handler (the next item stays Parked, the run starves after the head is
/// consumed).
#[test]
fn test_consumed_promotes_next() {
    let p = Arc::new(FakeProvider::text("ok"));
    let mut app = app_with_provider(p, ToolRegistry::new());
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("a".into()));
    app.pending.push(PendingItem::ParkedMessage("b".into()));
    app.handle_agent_message(AgentMessage::QueueConsumed {
        texts: vec!["a".into()],
    });
    assert_eq!(
        app.pending,
        vec![PendingItem::Message("b".into())],
        "b promoted into the live-copy slot after a was consumed"
    );
    assert_eq!(app.pending.len(), 1, "only b remains, live");
}
