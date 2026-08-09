//! Queue-path invariants for spawn_run. Split out of run_control_tests so
//! that file stays under the size gate; this file holds the regression tests
//! for the spawn_run queue path (a second Enter while agent_busy).

use super::run_control_tests::app_with_provider;
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

/// Cross-swap / cross-interrupt FIFO invariant: a ParkedMessage ahead in the
/// queue (a message carried across a swap, or one orphaned by an interrupt
/// or /clear -- both demoted to ParkedMessage because the server queue is
/// empty) must BLOCK InjectUser of a message enqueued after it. Without the
/// barrier, the new message would get a server copy + be consumed mid-turn
/// (QueueConsumed) BEFORE the parked one runs -- a FIFO reversal across the
/// host/server split. The barrier treats a ParkedMessage ahead like a
/// Command: the new message parks too (no InjectUser), so both drain in host
/// FIFO order as follow-up runs.
#[test]
fn test_parked_message_blocks_inject() {
    let p = Arc::new(FakeProvider::text("ok"));
    let mut app = app_with_provider(p, ToolRegistry::new());
    app.agent_busy = true;
    // A message already parked ahead (e.g. carried across a swap, demoted
    // because the new runner's server queue is empty).
    app.pending
        .push(PendingItem::ParkedMessage("carried".into()));
    // A second Enter while busy: must NOT InjectUser past the parked head.
    app.spawn_run("newcomer".into());
    assert_eq!(
        app.pending.len(),
        2,
        "second input queued behind the parked head"
    );
    assert_eq!(
        app.pending[1],
        PendingItem::ParkedMessage("newcomer".into()),
        "new message parks (no InjectUser) so it cannot leapfrog the parked \
         head mid-turn; both drain in host FIFO order"
    );
    // Contrast: once the head is a Message (live server copy, in-server), a new
    // message InjectUser's behind it -- FIFO preserved in the server queue.
    let mut app2 = app_with_provider(Arc::new(FakeProvider::text("ok")), ToolRegistry::new());
    app2.agent_busy = true;
    app2.pending.push(PendingItem::Message("injected".into()));
    app2.spawn_run("newcomer".into());
    assert_eq!(
        app2.pending[1],
        PendingItem::Message("newcomer".into()),
        "a Message head (live copy) is not a barrier; the new message joins \
         the server queue behind it (FIFO)"
    );
}
