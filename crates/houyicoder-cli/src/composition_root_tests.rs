//! Tests for the composition root: host lease lock ordering, runner
//! reclamation on exit and unwind, and the child-join-before-unlock
//! contract.

use super::*;
use crate::session_lock::{LockError, SessionLock};
use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnHandle, SpawnOutcome};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;
use houyicoder_context::SessionId;
use houyicoder_service::composition::{BuildRunnerOptions, build_runner};
use std::env;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use tokio::runtime::Builder;
use tokio::sync::oneshot::channel;
use tokio::task::yield_now;

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = env::temp_dir().join(format!("host-lease-{}-{n}", process::id()));
    fs::create_dir_all(&p).expect("temp root");
    p
}

/// A spawn handle whose join_children waits on a test gate, standing in
/// for a child driver still finalizing. The release task must hold the
/// session lock until this join resolves.
struct GatedJoin {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl SpawnHandle for GatedJoin {
    fn spawn(
        &self,
        _ctx: &ToolCtx,
        _args: SpawnArgs,
    ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
        Box::pin(async { Err(SpawnFailure::Recursive) })
    }

    fn join_children(&self) -> PFut<'_, ()> {
        self.entered.notify_one();
        let release = self.release.clone();
        Box::pin(async move {
            release.notified().await;
        })
    }
}

/// The release task joins the lease's children before dropping the lock:
/// while a child's join is still pending (the gated stand-in), a second
/// acquire reports the lock held; only after the join resolves does the
/// unlock happen. A release that dropped the lock after only the host
/// tasks — or at task entry — would let the first acquire in early.
#[cfg(unix)]
#[test]
fn test_release_joins_children_first() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let root = temp_root();
        let sid_str = SessionId::new().to_string();
        let lock = SessionLock::acquire(&sid_str, &root).expect("first acquire");

        let mut host = HostLifecycle::new();
        host.track(tokio::spawn(async {}));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release_gate = Arc::new(tokio::sync::Notify::new());
        let runner = build_runner(BuildRunnerOptions::default())
            .runner
            .with_spawn_handle(Arc::new(GatedJoin {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release_gate),
            }));
        let runner = Arc::new(runner);
        let lease = HostLease::holding(host, lock, sid_str.clone(), Some(runner));
        let release = lease.begin_release(runtime.handle());
        // Wait until the release task reached the child join: the lock
        // must still be held there, before the gate opens.
        entered.notified().await;
        tokio::task::yield_now().await;
        match SessionLock::acquire(&sid_str, &root) {
            Err(LockError::HeldByOther(_)) => {}
            Ok(_) => panic!("lock released while the child join was pending"),
            Err(LockError::Io(e)) => panic!("unexpected io error: {e}"),
        }
        release_gate.notify_one();
        let report = release.await.expect("release task");
        assert_eq!(report.panic_count(), 0, "no writer panicked");
        let _reacquire =
            SessionLock::acquire(&sid_str, &root).expect("lock released only after the child join");
        fs::remove_dir_all(&root).ok();
    });
}

/// The lock stays held while a writer the host tracks is still running,
/// even after the release task has started. The cancel ack proves
/// begin_shutdown ran; a yield then confirms the release task entered
/// its wait, so a second acquire must report the lock held. After the
/// writer finishes, the release drops the lock and a second acquire
/// succeeds — a release that dropped the lock at task entry would let
/// that first acquire in early.
#[cfg(unix)]
#[test]
fn test_lock_outlives_writer() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let root = temp_root();
        let sid = SessionId::new();
        let sid_str = sid.to_string();
        let lock = SessionLock::acquire(&sid_str, &root).expect("first acquire");

        let mut host = HostLifecycle::new();
        let cancel = host.cancel_token();
        let (seen_tx, seen_rx) = channel::<()>();
        let (release_tx, release_rx) = channel::<()>();
        let done = Arc::new(AtomicBool::new(false));
        let done_c = Arc::clone(&done);
        host.track(tokio::spawn(async move {
            // Stand in for a drain writer: acknowledge the cancel so the
            // test knows begin_shutdown ran, then wait on a second gate so
            // the writer stays alive until the test lets it finish.
            let _ = cancel.cancelled().await;
            let _ = seen_tx.send(());
            let _ = release_rx.await;
            done_c.store(true, Ordering::SeqCst);
        }));

        let lease = HostLease {
            host,
            lock: Some(lock),
            sid: Some(sid_str.clone()),
            children: None,
        };
        let release = lease.begin_release(runtime.handle());

        // The cancel ack proves begin_shutdown ran, not that the release
        // task entered its wait — the cancel fires before the task
        // exists. One yield closes the gap: the current-thread scheduler
        // polls every ready task before resuming, so the release task
        // entered its wait by now.
        let _ = seen_rx.await;
        yield_now().await;
        match SessionLock::acquire(&sid_str, &root) {
            Err(LockError::HeldByOther(_)) => {}
            Ok(_) => panic!("lock released while writer still running"),
            Err(LockError::Io(e)) => panic!("unexpected io error: {e}"),
        }

        // Let the writer finish; the release task joins it and drops the
        // lock only after. A second acquire must now succeed.
        drop(release_tx);
        let report = release.await.expect("release task");
        assert_eq!(report.panic_count(), 0, "no writer panicked");

        let _reacquire = SessionLock::acquire(&sid_str, &root).expect("acquire after release");
        assert!(
            done.load(Ordering::SeqCst),
            "writer finished before lock drop"
        );
        fs::remove_dir_all(&root).ok();
    });
}

/// A panic unwinding past the run loop must not drop the lease while its
/// writers still run: the guard's drop releases it mid-unwind, so the
/// writer is cancelled and joined before the lock drops. Without that
/// drop the cancel never fires and the done flag never sets.
#[cfg(unix)]
#[test]
fn test_unwind_releases_lease() {
    let root = temp_root();
    let sid = SessionId::new();
    let sid_str = sid.to_string();
    let lock = SessionLock::acquire(&sid_str, &root).expect("first acquire");

    let mut host = HostLifecycle::new();
    let cancel = host.cancel_token();
    let done = Arc::new(AtomicBool::new(false));
    let done_c = Arc::clone(&done);
    let handle = shared_runtime().handle().clone();
    host.track(handle.spawn(async move {
        let _ = cancel.cancelled().await;
        done_c.store(true, Ordering::SeqCst);
    }));

    let slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(HostLease {
        host,
        lock: Some(lock),
        sid: Some(sid_str.clone()),
        children: None,
    })));

    // Controlled unwind: the guard drops mid-panic, as in a run-loop
    // panic. AssertUnwindSafe is sound — it only releases what it owns.
    let guard = LeaseScopeGuard::new(&slot);
    let unwind = catch_unwind(AssertUnwindSafe(|| {
        let _held = guard;
        panic!("run loop failed");
    }));
    assert!(unwind.is_err(), "the run loop panic was captured");

    assert!(done.load(Ordering::SeqCst), "writer joined during unwind");
    let _reacquire = SessionLock::acquire(&sid_str, &root).expect("lock released in order");
    fs::remove_dir_all(&root).ok();
}

/// Build one live session through the real composition. build_runner
/// always provides a real bus, so start_local_server starts the serve,
/// fleet relay, notification drain, and refresh tasks. Dropping the
/// client lets the serve exit — keeping it parks the serve.
fn start_host(
    root: &Path,
) -> (
    Arc<Runner>,
    Client,
    Weak<Runner>,
    HostLifecycle,
    SessionLock,
) {
    let bundle = build_runner(BuildRunnerOptions::default());
    let sid = bundle.session.to_string();
    let (runner, client, _warnings, host) = start_local_server(
        bundle.runner,
        bundle.session,
        bundle.gate,
        bundle.sandbox_session,
        bundle.append_notify,
        None,
        None,
        bundle.bus,
    );
    let weak = Arc::downgrade(&runner);
    let lock = SessionLock::acquire(&sid, root).expect("acquire session lock");
    (runner, client, weak, host, lock)
}

/// The real bus means the host tracks four tasks — serve, fleet relay,
/// notification drain, and model refresh — and shutdown joins all four
/// cleanly. A bus-less fixture would track only the serve and refresh, so
/// this asserts the relay and drain really ran, not a stub stand-in.
#[test]
fn test_bus_tasks_joined() {
    let root = temp_root();
    let (runner, client, _weak, host, _lock) = start_host(&root);
    drop(client);
    drop(runner);
    let rt = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    let report = rt.block_on(host.begin_shutdown());
    assert_eq!(report.panic_count(), 0, "no task panicked during shutdown");
    assert_eq!(
        report.outcomes().len(),
        4,
        "serve, relay, drain, and refresh all tracked"
    );
    fs::remove_dir_all(&root).ok();
}

/// A normal exit joins the current lease and every prior switch's
/// still-pending release, reclaiming each session's runner Arc. The first
/// session's client stays parked through exit, so its serve cannot end on
/// its own — reclaiming it proves the pending release is force-joined,
/// not abandoned. The second session's client drops first, the clean path.
#[test]
fn test_exit_reclaims_runners() {
    let root = temp_root();
    let (runner0, _client0, weak0, host0, lock0) = start_host(&root);
    let slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(HostLease {
        host: host0,
        lock: Some(lock0),
        sid: Some("switch-0".into()),
        children: None,
    })));
    let (runner1, client1, weak1, host1, lock1) = start_host(&root);
    slot.lock().expect("scope").install(
        host1,
        lock1,
        "switch-1".into(),
        None,
        shared_runtime().handle(),
    );
    drop(client1);
    drop(runner1);
    drop(runner0);

    assert!(weak0.upgrade().is_some(), "parked serve holds its runner");
    release_all(&slot);
    assert!(weak0.upgrade().is_none(), "prior runner reclaimed on exit");
    assert!(
        weak1.upgrade().is_none(),
        "current runner reclaimed on exit"
    );
    fs::remove_dir_all(&root).ok();
}

/// A panic unwinding past the run loop drops the guard, which releases the
/// current and prior leases mid-unwind. The first session's client stays
/// parked so its release is still pending when the panic trips; the
/// guard's drop must join it. Both runners are reclaimed afterward.
#[test]
fn test_unwind_reclaims_runners() {
    let root = temp_root();
    let (runner0, _client0, weak0, host0, lock0) = start_host(&root);
    let slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(HostLease {
        host: host0,
        lock: Some(lock0),
        sid: Some("switch-0".into()),
        children: None,
    })));
    let (runner1, client1, weak1, host1, lock1) = start_host(&root);
    slot.lock().expect("scope").install(
        host1,
        lock1,
        "switch-1".into(),
        None,
        shared_runtime().handle(),
    );
    drop(client1);
    drop(runner1);
    drop(runner0);

    let guard = LeaseScopeGuard::new(&slot);
    let unwind = catch_unwind(AssertUnwindSafe(|| {
        let _held = guard;
        panic!("run loop failed");
    }));
    assert!(unwind.is_err(), "the run loop panic was captured");

    assert!(
        weak0.upgrade().is_none(),
        "prior runner reclaimed on unwind"
    );
    assert!(
        weak1.upgrade().is_none(),
        "current runner reclaimed on unwind"
    );
    fs::remove_dir_all(&root).ok();
}
