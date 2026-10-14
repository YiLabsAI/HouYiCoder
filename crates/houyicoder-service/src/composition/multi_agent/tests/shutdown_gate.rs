//! Shutdown-versus-spawn races: the completion gate must hold every child
//! the parent created, including ones whose driver has not registered yet,
//! and a stop must reach a child before its first poll.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnHandle};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, EventId, SessionEvent,
    SessionId, SessionLogEntry,
};
use houyicoder_core::agent::ToolRegistry;
use houyicoder_core::agent::multi_agent::concurrency_gate::ConcurrencyGate;
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};
use houyicoder_provider::FakeProvider;
use tokio::sync::Notify;
use tokio::task::yield_now;
use tokio::time::timeout;

use super::{runtime_with, runtime_with_backend};
use futures::stream::once;

/// A provider that parks the child in its model call and only returns once
/// the test releases it, so the finalize tail is observably late.
struct GateProvider {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

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
                model: "test".to_string(),
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

/// A spawn that has returned but whose driver has not registered yet must not
/// slip past a shutdown that starts immediately after: the current join
/// drains only what the map holds at call time, so a child between
/// background_started and register has no entry to join, and its
/// SubagentReturn can land after the join resolved. The join must cover the
/// child the parent was told about, not the ones the registry happened to
/// hold when it looked.
#[tokio::test]
async fn test_shutdown_races_spawn_registration() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let provider = Arc::new(GateProvider {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let (runtime, store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    runtime
        .spawn(&ctx, args)
        .await
        .expect("background spawn launches");

    // Shutdown immediately: no yield, no wait for the child to enter its
    // model call. The driver task has been spawned but its registry entry
    // may not exist yet, and the stop request has not reached the child.
    runtime.join_children().await;

    let events = store.trajectory_snapshot(parent_sid);
    let has_return = events
        .iter()
        .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. }));
    assert!(
        has_return,
        "the spawn the parent observed must be finalized before the join resolves"
    );
}

/// A stop issued before the child's driver first polls must still bring the
/// child to a terminal: the run token is minted on first poll, so a stop that
/// only aborts a live runner finds no token to cancel and the child parks
/// forever. The join resolves only after every observed child reached its
/// finalize tail, so this test hangs rather than passing silently if the
/// pre-start stop is unreachable.
#[tokio::test]
async fn test_shutdown_stops_unstarted_child() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let provider = Arc::new(GateProvider {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let (runtime, store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    runtime
        .spawn(&ctx, args)
        .await
        .expect("background spawn launches");

    // Stop before yielding to the driver: the child has not entered its run.
    runtime.join_children().await;

    let events = store.trajectory_snapshot(parent_sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "an unstarted child still reaches a terminal on shutdown"
    );
    // The child never entered its model call, so nothing was released late.
    drop(release);
}

/// A slow finalize tail must hold the join open: the child entered its model
/// call, the stop cancels it, and the worktree/return work after the run
/// takes longer than a fixed budget. The join resolving early would mean the
/// parent log is still being written while the caller believes shutdown is
/// complete. This pins the bounded-wait contract from the waiting side: the
/// budget is per task and generous for real cleanup, so a normal-speed tail
/// finishes inside it.
#[tokio::test]
async fn test_shutdown_waits_finalize() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let provider = Arc::new(GateProvider {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let (runtime, store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    runtime
        .spawn(&ctx, args)
        .await
        .expect("background spawn launches");

    // Let the child enter the model call, then stop: the run observes the
    // cancel and reaches its terminal, after which finalize_child writes the
    // return boundary. The join must not resolve before that write.
    entered.notified().await;
    runtime.join_children().await;

    let events = store.trajectory_snapshot(parent_sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "the finalize tail landed before the join resolved"
    );
}

/// A backend that parks the first append a child makes (the delegation write
/// that opens its sidechain log) until released, signalling when that write
/// is reached. It lets a shutdown or caller cancel land while the driver sits
/// inside construction, before any child is registered, so those two
/// admission windows are exercised end to end.
struct BlockingChildWrite {
    inner: InMemoryBackend,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    blocked: AtomicBool,
}

impl BlockingChildWrite {
    fn new(entered: Arc<Notify>, release: Arc<Notify>) -> Self {
        Self {
            inner: InMemoryBackend::new(),
            entered,
            release,
            blocked: AtomicBool::new(false),
        }
    }
}

impl ContextBackend for BlockingChildWrite {
    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        if !self.blocked.swap(true, Ordering::SeqCst) {
            self.entered.notify_one();
            let release = self.release.clone();
            let inner = &self.inner;
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

/// A provider that parks the child forever, so the child's only route to a
/// terminal is a shutdown cancel. Used where a normal completion would hide
/// a failed cancel: the return boundary would still land, but for the wrong
/// reason.
struct ParkedProvider;

impl ModelProvider for ParkedProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(std::future::pending())
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        Box::pin(futures::stream::pending())
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A backend whose append panics, so the child's construction — its opening
/// delegation write inside spawn_child — unwinds before the driver reports
/// ready. The resulting refusal must be a failed start, not a false "host is
/// shutting down": a construction panic in a healthy host is not a shutdown.
struct PanickingBackend {
    inner: InMemoryBackend,
}

impl ContextBackend for PanickingBackend {
    fn append(&self, _event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        panic!("PanickingBackend::append")
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

/// A shutdown landing while the child is still under construction must still
/// bring it to a terminal: the close credential was admitted before the first
/// side effect, so the join cancels it and the born child (whose cancel token
/// is that same credential) reaches its return boundary even though the
/// driver had not registered it when the shutdown fired.
#[tokio::test]
async fn test_shutdown_covers_construction() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let backend = Box::new(BlockingChildWrite::new(
        Arc::clone(&entered),
        Arc::clone(&release),
    ));
    let provider: Arc<dyn ModelProvider> = Arc::new(ParkedProvider);
    let (runtime, store, parent_sid) =
        runtime_with_backend(backend, provider, None, ToolRegistry::new());
    let runtime = Arc::new(runtime);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;

    // The driver parks inside the delegation write, and the caller is held on
    // ready, so the spawn runs on its own task and the join on this one.
    let spawn_task = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            let ctx = ToolCtx::new("c1").with_session(parent_sid);
            runtime.spawn(&ctx, args).await
        })
    };
    entered.notified().await;

    // Shut down mid-construction, then free the write: the admitted close is
    // already cancelled, so the child that finishes constructing is cancelled
    // into its terminal before the join resolves. Wait until the join actually
    // cancelled the admitted close before freeing the write — freeing first
    // would let construction run ahead of the close and only prove a plain
    // shutdown, not that the close precedes construction.
    let join_task = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.join_children().await })
    };
    let gate_closed = async {
        loop {
            let closed = runtime
                .child_drivers
                .lock()
                .map(|g| g.closing)
                .unwrap_or(false);
            if closed {
                break;
            }
            yield_now().await;
        }
    };
    timeout(Duration::from_secs(1), gate_closed)
        .await
        .expect("the join closed the gate before construction freed");
    // Construction is still parked on the write, so the join is waiting on the
    // driver rather than finishing on the cancel alone.
    assert!(
        !join_task.is_finished(),
        "the join stays parked on the constructing driver, not finished on cancel alone"
    );
    release.notify_waiters();

    join_task.await.expect("the join resolves");
    drop(spawn_task.await);

    let events = store.trajectory_snapshot(parent_sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "the constructing child still reached a terminal before the join resolved"
    );
}

/// Cancelling the spawn caller while it waits for its child does not take the
/// child's construction with it: the driver owns construction and the
/// completion signal, so the child still runs to its return boundary once the
/// write frees. A caller-held construction would vanish on the abort and
/// never record a return.
#[tokio::test]
async fn test_caller_cancel_spares_child() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let backend = Box::new(BlockingChildWrite::new(
        Arc::clone(&entered),
        Arc::clone(&release),
    ));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("child answer"));
    let (runtime, store, parent_sid) =
        runtime_with_backend(backend, provider, None, ToolRegistry::new());
    let runtime = Arc::new(runtime);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;

    let spawn_task = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move {
            let ctx = ToolCtx::new("c1").with_session(parent_sid);
            runtime.spawn(&ctx, args).await
        })
    };
    entered.notified().await;

    // Cancel the caller at its ready wait, then free the write. The driver is
    // untouched, so it constructs, runs, and finalizes the child on its own.
    spawn_task.abort();
    release.notify_waiters();
    runtime.join_children().await;
    // The caller was held on its ready wait; aborting it must report a
    // cancellation, not a completed spawn. A completed spawn here would mean
    // the caller finished under some answer, so the child's survival below
    // would prove nothing about who owned the construction.
    let caller = spawn_task.await;
    assert!(
        matches!(&caller, Err(err) if err.is_cancelled()),
        "the aborted caller reports cancellation, not a completed spawn: {caller:?}"
    );

    let events = store.trajectory_snapshot(parent_sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "the child outlived its cancelled caller and still recorded its return"
    );
}

/// A spawn waiting for a concurrency slot when the shutdown closes the gate
/// is refused with ShuttingDown once the running child frees its slot —
/// never built after the drain.
#[tokio::test]
async fn test_shutdown_refuses_queued_spawn() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let provider = Arc::new(GateProvider {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });
    let (runtime, _store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let gate = Arc::new(ConcurrencyGate::new(1, 1));
    let runtime = runtime.with_gate(Arc::clone(&gate));
    let runtime = Arc::new(runtime);

    let first_args = {
        let ctx = ToolCtx::new("c1").with_session(parent_sid);
        let mut args = SpawnArgs::new("explore", "task", "task");
        args.run_in_background = true;
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.spawn(&ctx, args).await })
    };
    entered.notified().await;

    let second_args = {
        let ctx = ToolCtx::new("c1").with_session(parent_sid);
        let args = SpawnArgs::new("explore", "second", "second");
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.spawn(&ctx, args).await })
    };
    let gate_depth = async {
        loop {
            if gate.queued_count() == 1 {
                break;
            }
            yield_now().await;
        }
    };
    timeout(Duration::from_secs(1), gate_depth)
        .await
        .expect("the second spawn queued for the slot");

    runtime.join_children().await;

    let call = second_args.await.expect("the queued spawn's task ends");
    assert!(
        matches!(call, Err(SpawnFailure::ShuttingDown)),
        "the queued creation is refused with the honest shutdown verdict, got {call:?}"
    );
    drop(first_args.await);
    drop(release);
}

/// A construction panic — the child's opening delegation write unwinds before
/// ready — is a failed start, not a shutdown. The ready channel closed with no
/// child id and no construction error, and the gate is still open, so routing
/// it to ShuttingDown would tell the model the host is going away when only
/// the child failed to materialize.
#[tokio::test]
async fn test_construction_panic_failed_start() {
    let backend = Box::new(PanickingBackend {
        inner: InMemoryBackend::new(),
    });
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("never reached"));
    let (runtime, _store, parent_sid) =
        runtime_with_backend(backend, provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    let err = runtime
        .spawn(&ctx, args)
        .await
        .expect_err("a construction panic fails the spawn");
    assert!(
        matches!(err, SpawnFailure::ConstructionFailed),
        "a construction panic is a failed start, not a shutdown: {err:?}"
    );
}
