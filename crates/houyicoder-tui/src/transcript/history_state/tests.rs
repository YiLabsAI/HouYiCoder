//! What a history read's lifecycle promises: which events invalidate a
//! running read, what a dispatch stamps, and what the worker budget counts.

use super::*;
use std::thread;
use std::time::Duration;

/// A wait on the other side of a thread. Bounded so a worker that never
/// arrives fails the test instead of hanging it.
fn rendezvous<T>(channel: &mpsc::Receiver<T>) -> T {
    channel
        .recv_timeout(Duration::from_secs(5))
        .expect("the other side of the rendezvous arrived")
}

/// The invalidate drops the running read and moves the generation on, so
/// a held record from before is told apart by its stamp alone. The slot
/// outlives the record, because it belongs to the worker and nothing ran
/// here to give it back before the lease drops.
#[test]
fn test_invalidate_moves_epoch() {
    let mut reads = HistoryReads::default();
    let (tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    let lease = reads.dispatch(0, DiskFront::Unloaded, rx);
    assert_eq!(reads.running(), 1);
    let epoch_before = reads.epoch();

    reads.invalidate();

    assert!(
        !reads.holds_pending(),
        "the running read was dropped from file"
    );
    assert_ne!(reads.epoch(), epoch_before, "the generation moved on");
    // The dropped receiver makes the worker send fail; nothing panics.
    assert!(tx.send(HistoryReadOutcome::Exhausted).is_err());
    drop(lease);
    assert_eq!(reads.running(), 0, "the worker leaving frees the slot");
}

/// A dispatch after an invalidation carries the new generation and both
/// transcript coordinates, so the apply side compares against the history
/// the read actually saw.
#[test]
fn test_dispatch_stamps_epoch() {
    let mut reads = HistoryReads::default();
    reads.invalidate();
    let (_tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    let _lease = reads.dispatch(3, DiskFront::At(4096), rx);
    let pending = reads.take().expect("a read is pending");
    assert_eq!(pending.epoch, reads.epoch());
    assert_eq!(pending.dispatch_front, 3);
    assert_eq!(pending.dispatch_disk_front, DiskFront::At(4096));
}

/// An invalidation raises the flag the dispatched worker holds, which is the
/// half of the stop contract the owner owns: it cannot cancel a read already
/// in the kernel, so it tells the worker its rows are unwanted and the
/// worker quits at its next window. The first lease is scoped so it hands
/// its slot back before the second dispatch, which is what production does:
/// the worker leaves, then a later scroll asks again. The next dispatch gets
/// a fresh flag, because a raised one would stop the read it was just handed
/// to.
#[test]
fn test_invalidate_raises_stop() {
    let mut reads = HistoryReads::default();
    let (_tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    let stopped = {
        let lease = reads.dispatch(0, DiskFront::Unloaded, rx);
        let flag = Arc::clone(lease.stop());
        assert!(!flag.load(Ordering::Relaxed), "a fresh read is wanted");

        reads.invalidate();

        assert!(
            flag.load(Ordering::Relaxed),
            "the dropped read was told to stop, not merely unheard"
        );
        flag
    };

    let (_tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    let next = reads.dispatch(0, DiskFront::Unloaded, rx);
    assert!(
        !next.stop().load(Ordering::Relaxed),
        "a new read does not inherit the stop it was not given"
    );
    assert_ne!(
        Arc::as_ptr(&stopped),
        Arc::as_ptr(next.stop()),
        "each dispatch hands out its own flag"
    );
}

/// A record on file blocks a dispatch even with no worker running, because
/// dispatch replaces the record wholesale and that record may hold a result
/// nobody has read yet. The worker that produced it can already have left,
/// since it sends before it exits, so the budget alone cannot say whether a
/// dispatch is safe.
#[test]
fn test_record_blocks_dispatch() {
    let mut reads = HistoryReads::default();
    let (tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    drop(reads.dispatch(0, DiskFront::Unloaded, rx));
    assert_eq!(
        reads.running(),
        0,
        "the budget is free as soon as the lease goes"
    );
    assert!(
        !reads.may_dispatch(),
        "its record is still on file, so a dispatch would throw it away"
    );

    tx.send(HistoryReadOutcome::Exhausted).ok();
    assert!(
        !reads.may_dispatch(),
        "a result waiting in the channel blocks the dispatch that would drop it"
    );

    let taken = reads.take().expect("the record is on file");
    assert!(matches!(taken.poll(), HistoryReadPoll::Ready(_)));
    assert!(
        reads.may_dispatch(),
        "the record cleared, so the budget and the file agree again"
    );
}

/// The budget counts what is executing, so dropping the record does not open
/// a second slot while the first worker is still inside its read. This is
/// the interleaving production reaches: a dispatch, then a history clear
/// invalidating it, then a scroll that asks for older history again. A
/// worker stopped but not yet returned cannot observe its flag, so a second
/// walk started in that gap reads the same log beside the first, and the
/// first walk's bytes are read twice for one answer.
#[test]
fn test_running_counts_workers() {
    let mut reads = HistoryReads::default();
    let (_tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    let lease = reads.dispatch(0, DiskFront::Unloaded, rx);

    // Two rendezvous instead of sleeps: the worker announces it has arrived
    // inside its read, and the test releases it again only after checking
    // the count. That makes the ordering the claim depends on structural
    // rather than timed.
    let (arrived_tx, arrived_rx) = mpsc::sync_channel::<()>(0);
    let (release_tx, release_rx) = mpsc::sync_channel::<()>(0);
    let worker = thread::spawn(move || {
        arrived_tx.send(()).expect("the test is listening");
        rendezvous(&release_rx);
        assert!(
            lease.stop().load(Ordering::Relaxed),
            "the dropped record told this worker to stop"
        );
        drop(lease);
    });
    rendezvous(&arrived_rx);

    reads.invalidate();

    assert_eq!(reads.running(), 1, "a stopped worker still holds its slot");
    // The dispatch point asks this, not the count, so this is the assertion
    // that stands in for the second dispatch the interleaving would do. The
    // record is already gone, so only the budget term can refuse it.
    assert!(
        !reads.may_dispatch(),
        "the dispatch point refuses a second walk of the same log"
    );

    release_tx.send(()).expect("the worker is listening");
    worker.join().expect("the worker returns");
    assert_eq!(reads.running(), 0, "the worker leaving frees the slot");
}

mod events;
