//! Tests for local host leases, shutdown scopes, and unlock ordering.

use std::env::temp_dir;
#[cfg(target_os = "macos")]
use std::fs::read_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::id as process_id;
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

#[cfg(target_os = "macos")]
use houyicoder_api::provider::ModelProvider;
#[cfg(unix)]
use houyicoder_api::spawn::SpawnArgs;
#[cfg(unix)]
use houyicoder_api::tool::ToolCtx;
#[cfg(unix)]
use houyicoder_async::PFut;
#[cfg(target_os = "macos")]
use houyicoder_async::PStream;
use houyicoder_client::Client;
#[cfg(unix)]
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, EventId, SessionEvent,
    SessionId, SessionLogEntry,
};
use houyicoder_core::agent::Runner;
use houyicoder_core::agent::multi_agent::bus_types::AgentBus;
#[cfg(unix)]
use houyicoder_memory::InMemoryBackend;
#[cfg(target_os = "macos")]
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_service::composition::host_lifecycle::HostLifecycle;
use houyicoder_service::composition::{
    BuildRunnerOptions, ProviderFacts, ProviderResolution, build_runner,
};
use houyicoder_tui::composition::shared_runtime;
#[cfg(unix)]
use tokio::runtime::Builder;
#[cfg(unix)]
use tokio::spawn;
#[cfg(unix)]
use tokio::sync::Notify;
#[cfg(unix)]
use tokio::sync::oneshot::channel;
#[cfg(unix)]
use tokio::task::yield_now;

use super::*;
use crate::composition::start_local_server;
#[cfg(unix)]
use crate::session::lock::LockError;
use crate::session::lock::SessionLock;
#[cfg(target_os = "macos")]
use futures::stream::once;
#[cfg(target_os = "macos")]
use std::process::Command;

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("local-host-{}-{n}", process_id()));
    create_dir_all(&p).expect("temp root");
    p
}

/// Separate test modules must use distinct temp root prefixes so parallel
/// execution and cleanups in one module cannot mutate or delete another's
/// active lock directories or settings.
#[cfg(unix)]
#[test]
fn test_fixture_roots_isolated() {
    let local = temp_root();
    let other = temp_dir().join(format!("composition-host-{}-0", process_id()));
    create_dir_all(&other).expect("other dir");
    let sid = SessionId::new().to_string();
    let lock = SessionLock::acquire(&sid, &other).expect("lock in other dir");

    // Clean up the local module's directory; the other root and its held
    // lock must remain completely intact.
    remove_dir_all(&local).ok();
    assert!(other.exists(), "other module root survived cleanup");
    assert!(
        matches!(
            SessionLock::acquire(&sid, &other),
            Err(LockError::HeldByOther(_))
        ),
        "held lock in other root remained valid"
    );

    drop(lock);
    remove_dir_all(&other).ok();
}

/// A backend that parks when SubagentReturn is appended to the parent session
/// log, standing in for a child driver inside its finalize tail.
#[cfg(unix)]
struct LatchingBackend {
    inner: Arc<InMemoryBackend>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[cfg(unix)]
impl ContextBackend for LatchingBackend {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        if matches!(event.event, SessionEvent::SubagentReturn { .. }) {
            self.entered.notify_one();
            let release = self.release.clone();
            let inner = Arc::clone(&self.inner);
            return Box::pin(async move {
                release.notified().await;
                inner.append(event).await
            });
        }
        self.inner.append(event)
    }

    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.read_range(session, from, to)
    }

    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.replay(session)
    }

    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        self.inner.write_checkpoint(manifest)
    }

    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        self.inner.read_checkpoint(id)
    }

    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        self.inner.list_checkpoints(session)
    }
}

/// The release task joins real children before dropping the lock: while a child
/// driver is still inside its finalize tail appending SubagentReturn to the
/// parent session log, a second acquire reports the lock held; only after the
/// child finishes and the return event lands does the unlock happen.
#[cfg(unix)]
#[test]
fn test_release_joins_children_first() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let root = temp_root();
        let entered = Arc::new(Notify::new());
        let release_gate = Arc::new(Notify::new());
        let backend = Box::new(LatchingBackend {
            inner: Arc::new(InMemoryBackend::new()),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release_gate),
        });

        let mut options = BuildRunnerOptions::default();
        options.backend = Some(backend);
        options.provider = Some(ProviderResolution::stub());
        let bundle = build_runner(options);
        let sid_str = bundle.session.to_string();
        let lock = SessionLock::acquire(&sid_str, &root).expect("first acquire");

        let parent_sid = bundle.session;
        let (runner, client, _warnings, host) =
            start_local_server(bundle, None, Some(root.join("settings.json")));
        let spawn_handle = runner.spawn_handle().expect("real spawn handle attached");
        let ctx = ToolCtx::new("probe-parent").with_session(parent_sid);
        let mut args = SpawnArgs::new("explore", "probe-task", "probe-task");
        args.run_in_background = true;
        let outcome = spawn_handle
            .spawn(&ctx, args)
            .await
            .expect("spawn background child");
        assert!(
            outcome.status.is_none(),
            "background child has no status yet"
        );
        assert!(!outcome.child_session_id.is_empty(), "child id assigned");

        drop(client);
        let lease = HostLease::holding(host, lock, sid_str.clone(), Some(Arc::clone(&runner)));
        let release = lease.begin_release(runtime.handle());

        // Wait until the child reaches its finalize tail and parks on
        // SubagentReturn append.
        entered.notified().await;
        yield_now().await;
        match SessionLock::acquire(&sid_str, &root) {
            Err(LockError::HeldByOther(_)) => {}
            Ok(_) => panic!("lock released while the child finalize tail was pending"),
            Err(LockError::Io(e)) => panic!("unexpected io error: {e}"),
        }

        // The SubagentReturn event is not committed yet while the latch holds.
        let events_during = runner.store().trajectory_snapshot(parent_sid);
        assert!(
            !events_during
                .iter()
                .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
            "return event landed before latch release"
        );

        // Open the latch so the child driver completes its finalize tail.
        release_gate.notify_one();
        let report = release.await.expect("release task");
        assert_eq!(report.panic_count(), 0, "no writer panicked");

        let _reacquire =
            SessionLock::acquire(&sid_str, &root).expect("lock released only after child finished");

        let events_after = runner.store().trajectory_snapshot(parent_sid);
        assert!(
            events_after
                .iter()
                .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
            "the finalize tail landed before the release resolved"
        );

        drop(runner);
        remove_dir_all(&root).ok();
    });
}

#[cfg(target_os = "macos")]
fn make_test_repo(tag: &str) -> PathBuf {
    let dir = temp_dir().join(format!("houyi-lease-wt-{}-{tag}", process_id()));
    drop(remove_dir_all(&dir));
    create_dir_all(&dir).expect("repo dir");
    #[expect(clippy::disallowed_methods, reason = "test scaffolding git init")]
    {
        drop(
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["init", "-q"])
                .status(),
        );
        drop(
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["config", "user.email", "t@x"])
                .status(),
        );
        drop(
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["config", "user.name", "t"])
                .status(),
        );
        drop(
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["commit", "--allow-empty", "-m", "init", "-q"])
                .status(),
        );
    }
    dir
}

#[cfg(target_os = "macos")]
struct GateProvider {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[cfg(target_os = "macos")]
impl ModelProvider for GateProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.entered.notify_one();
        let release = self.release.clone();
        Box::pin(async move {
            release.notified().await;
            Ok(CompletionResponse {
                output: vec![OutputItem::Text {
                    text: "done".to_string(),
                }],
                usage: Usage::default(),
                model: "stub".to_string(),
            })
        })
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        self.entered.notify_one();
        let release = self.release.clone();
        Box::pin(once(async move {
            release.notified().await;
            Err(ProviderError::Network)
        }))
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// The release task holds the session lock until a child's worktree cleanup
/// is complete: while the child is still running in its worktree, a second
/// acquire reports the lock held; only after cleanup removes the worktree and
/// the finalize tail completes does the unlock happen. A worktree spawn
/// needs the sandbox to narrow its fence at runtime, which only the seatbelt
/// backend implements today; on other platforms the spawn is refused before
/// the lease path this test covers ever runs.
#[cfg(target_os = "macos")]
#[test]
fn test_release_waits_worktree_cleanup() {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    runtime.block_on(async {
        let repo = make_test_repo("1");
        let root = temp_root();
        let entered = Arc::new(Notify::new());
        let release_gate = Arc::new(Notify::new());
        let provider = Arc::new(GateProvider {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release_gate),
        });

        let mut options = BuildRunnerOptions::default();
        options.project = Some(repo.to_string_lossy().to_string());
        options.provider = Some(ProviderResolution {
            provider,
            warnings: Vec::new(),
            facts: ProviderFacts::default(),
        });
        let bundle = build_runner(options);
        let sid_str = bundle.session.to_string();
        let lock = SessionLock::acquire(&sid_str, &root).expect("first acquire");

        let parent_sid = bundle.session;
        let (runner, client, _warnings, host) =
            start_local_server(bundle, None, Some(root.join("settings.json")));
        let spawn_handle = runner.spawn_handle().expect("real spawn handle attached");
        let ctx = ToolCtx::new("probe-parent").with_session(parent_sid);
        let mut args = SpawnArgs::new("explore", "probe-task", "probe-task");
        args.isolation = "worktree".to_string();
        args.run_in_background = true;
        let outcome = spawn_handle
            .spawn(&ctx, args)
            .await
            .expect("spawn background child");
        assert!(outcome.status.is_none());
        assert!(!outcome.child_session_id.is_empty());

        let worktrees_dir = repo.join(".houyicoder").join("worktrees");
        // Wait until the child is actively executing inside its worktree.
        entered.notified().await;

        // While running, the worktree exists on disk.
        assert_eq!(
            read_dir(&worktrees_dir).map(|d| d.count()).unwrap_or(0),
            1,
            "child worktree exists on disk during child execution"
        );

        drop(client);
        let lease = HostLease::holding(host, lock, sid_str.clone(), Some(Arc::clone(&runner)));
        let release = lease.begin_release(runtime.handle());

        yield_now().await;
        match SessionLock::acquire(&sid_str, &root) {
            Err(LockError::HeldByOther(_)) => {}
            Ok(_) => panic!("lock released while child worktree was still present"),
            Err(LockError::Io(e)) => panic!("unexpected io error: {e}"),
        }

        // Release the child so it completes and cleans up the worktree.
        release_gate.notify_one();
        let report = release.await.expect("release task");
        assert_eq!(report.panic_count(), 0, "no writer panicked");

        let _reacquire =
            SessionLock::acquire(&sid_str, &root).expect("lock released only after cleanup");

        // The worktree directory must be cleaned up and empty.
        assert_eq!(
            read_dir(&worktrees_dir).map(|d| d.count()).unwrap_or(0),
            0,
            "child worktree was reclaimed before lock was released"
        );

        drop(runner);
        remove_dir_all(&repo).ok();
        remove_dir_all(&root).ok();
    });
}

/// The lock stays held while a writer the host tracks is still running,
/// even after the release task has started. The cancel ack proves
/// begin_shutdown ran; a yield then confirms the release task entered
/// its wait, so a second acquire must report the lock held. After the
/// writer finishes, the release drops the lock and a second acquire
/// succeeds -- a release that dropped the lock at task entry would let
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
        host.track(spawn(async move {
            // Stand in for a drain writer: acknowledge the cancel so the
            // test knows begin_shutdown ran, then wait on a second gate so
            // the writer stays alive until the test lets it finish.
            let _ = cancel.cancelled().await;
            let _ = seen_tx.send(());
            let _ = release_rx.await;
            done_c.store(true, Ordering::SeqCst);
        }));

        let lease = HostLease::for_test(host, Some(lock), Some(sid_str.clone()));
        let release = lease.begin_release(runtime.handle());

        // The cancel ack proves begin_shutdown ran, not that the release
        // task entered its wait -- the cancel fires before the task
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
        remove_dir_all(&root).ok();
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

    let slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(
        HostLease::for_test(host, Some(lock), Some(sid_str.clone())),
    )));

    // Controlled unwind: the guard drops mid-panic, as in a run-loop
    // panic. AssertUnwindSafe is sound -- it only releases what it owns.
    let guard = LeaseScopeGuard::new(&slot);
    let unwind = catch_unwind(AssertUnwindSafe(|| {
        let _held = guard;
        panic!("run loop failed");
    }));
    assert!(unwind.is_err(), "the run loop panic was captured");

    assert!(done.load(Ordering::SeqCst), "writer joined during unwind");
    let _reacquire = SessionLock::acquire(&sid_str, &root).expect("lock released in order");
    remove_dir_all(&root).ok();
}

fn start_host_for_test(
    root: &Path,
) -> (
    Arc<Runner>,
    Client,
    Weak<Runner>,
    HostLifecycle,
    SessionLock,
    Arc<AgentBus>,
) {
    let settings_path = root.join("settings.json");
    let mut options = BuildRunnerOptions::default();
    options.provider = Some(ProviderResolution::stub());
    let bundle = build_runner(options);
    let sid = bundle.session.to_string();
    let bus = bundle.bus.clone().expect("the real assembly carries a bus");
    let (runner, client, _warnings, host) = start_local_server(bundle, None, Some(settings_path));
    let weak = Arc::downgrade(&runner);
    let lock = SessionLock::acquire(&sid, root).expect("acquire session lock");
    (runner, client, weak, host, lock, bus)
}

/// A normal exit joins the current lease and every prior switch's
/// still-pending release, reclaiming each session's runner Arc. The first
/// session's client stays parked through exit, so its serve cannot end on
/// its own -- reclaiming it proves the pending release is force-joined,
/// not abandoned. The second session's client drops first, the clean path.
#[test]
fn test_exit_reclaims_runners() {
    let root = temp_root();
    let (runner0, _client0, weak0, host0, lock0, _bus0) = start_host_for_test(&root);
    let slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(
        HostLease::for_test(host0, Some(lock0), Some("switch-0".into())),
    )));
    let (runner1, client1, weak1, host1, lock1, _bus1) = start_host_for_test(&root);
    slot.lock().expect("scope").install(
        HostLease::holding(host1, lock1, "switch-1".into(), None),
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
    remove_dir_all(&root).ok();
}

/// A panic unwinding past the run loop drops the guard, which releases the
/// current and prior leases mid-unwind. The first session's client stays
/// parked so its release is still pending when the panic trips; the
/// guard's drop must join it. Both runners are reclaimed afterward.
#[test]
fn test_unwind_reclaims_runners() {
    let root = temp_root();
    let (runner0, _client0, weak0, host0, lock0, _bus0) = start_host_for_test(&root);
    let slot: HostLeaseSlot = Arc::new(Mutex::new(HostLeaseScope::with_current(
        HostLease::for_test(host0, Some(lock0), Some("switch-0".into())),
    )));
    let (runner1, client1, weak1, host1, lock1, _bus1) = start_host_for_test(&root);
    slot.lock().expect("scope").install(
        HostLease::holding(host1, lock1, "switch-1".into(), None),
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
    remove_dir_all(&root).ok();
}
