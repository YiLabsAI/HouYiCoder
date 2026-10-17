//! Child-driver-completion tests: join_children stops every live child, joins
//! each detached background driver to its finalize tail, and survives a child
//! driver that panics.

use std::sync::Arc;

use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::spawn::{SpawnArgs, SpawnHandle};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::{HookEventKind, HookFirePayload, SessionEvent};
use houyicoder_core::agent::ToolRegistry;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, ProviderError,
};
use houyicoder_provider::FakeProvider;
use tokio::sync::Notify;

use super::runtime_with;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;

/// A provider whose stream never yields, so a child running against it parks
/// inside its model call. The first entry signals the test so it waits until
/// the child has actually reached the call before joining shutdown.
struct HangProvider {
    entered: Arc<Notify>,
}

impl ModelProvider for HangProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        self.entered.notify_one();
        Box::pin(std::future::pending())
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        self.entered.notify_one();
        Box::pin(futures::stream::pending())
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A background child parked in a never-returning model call still completes
/// before shutdown resolves: join_children aborts the child, joins its
/// detached driver, and the driver's finalize tail writes the SubagentReturn
/// boundary into the parent log. A stop request alone would return while the
/// child's completion tail had still not run.
#[tokio::test]
async fn test_join_children_awaits_completion() {
    let entered = Arc::new(Notify::new());
    let provider = Arc::new(HangProvider {
        entered: Arc::clone(&entered),
    });
    let (runtime, store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    runtime
        .spawn(&ctx, args)
        .await
        .expect("background spawn launches");
    // Wait for the child to actually enter the model call: aborting before
    // then would fire before the run has minted its cancel token and could
    // not interrupt it, so the driver would stay parked forever.
    entered.notified().await;
    runtime.join_children().await;
    let events = store.trajectory_tail(parent_sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "join_children awaited the child driver to its completion tail"
    );
}

/// A provider that panics on its stream entry, so a child run against it
/// unwinds. finalize_child catches the panic, runs its tail, then re-raises,
/// so the detached driver itself panics — the branch shutdown must survive.
struct PanickingProvider {
    entered: Arc<Notify>,
}

impl ModelProvider for PanickingProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        panic!("PanickingProvider::complete")
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        self.entered.notify_one();
        panic!("PanickingProvider::stream")
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A child whose run panics still finalizes (its return is recorded) before
/// the join resolves, and the join survives the panicked driver: it warns and
/// returns instead of re-raising the panic into the host teardown.
#[tokio::test]
async fn test_join_children_survives_panic() {
    let entered = Arc::new(Notify::new());
    let provider = Arc::new(PanickingProvider {
        entered: Arc::clone(&entered),
    });
    let (runtime, store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    runtime
        .spawn(&ctx, args)
        .await
        .expect("background spawn launches");
    // The child reached and unwound its model call; wait so the panic is the
    // outcome the join awaits rather than a race against abort.
    entered.notified().await;
    runtime.join_children().await;
    let events = store.trajectory_tail(parent_sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "the panicked child's finalize tail still records its return before the join resolves"
    );
}

/// A Stop hook that never returns cannot wedge the shutdown: the join's
/// budget expires, the driver is aborted, and no return boundary is written
/// for the aborted child.
#[tokio::test]
async fn test_shutdown_aborts_slow_hook() {
    let stop_entered = Arc::new(Notify::new());
    let (runtime, store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("done")),
        None,
        ToolRegistry::new(),
    );
    let runtime = Arc::new(runtime);
    let stop_gate = Arc::new(Notify::new());
    struct StopHook {
        stop_entered: Arc<Notify>,
        park: Arc<Notify>,
    }
    impl HookFire for StopHook {
        fn fire(&self, event: HookEventKind, _payload: HookFirePayload) -> PFut<'_, ()> {
            let stop_entered = self.stop_entered.clone();
            let park = self.park.clone();
            Box::pin(async move {
                if event == HookEventKind::SubagentStop {
                    stop_entered.notify_one();
                    park.notified().await;
                }
            })
        }
    }
    let ctx = ToolCtx::new("c1")
        .with_session(parent_sid)
        .with_hook_fire(Arc::new(StopHook {
            stop_entered: Arc::clone(&stop_entered),
            park: Arc::clone(&stop_gate),
        }) as Arc<dyn HookFire>);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    let spawn_call = {
        let runtime = Arc::clone(&runtime);
        let ctx = ctx.clone();
        tokio::spawn(async move { runtime.spawn(&ctx, args).await })
    };
    stop_entered.notified().await;

    runtime.join_children().await;

    let events = store.trajectory_tail(parent_sid);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "an aborted slow-hook driver fabricates no return boundary"
    );
    drop(spawn_call.await);
    // The stop hook park is still held by nothing: releasing it (or dropping
    // the Notify) lets any late waiter finish, so the test runtime does not
    // wait on it at teardown.
    drop(stop_gate);
}

/// A poisoned children lock still yields a full enumeration rather than an
/// empty close: the join recovers the lock, cancels every registered child,
/// and brings the parked child to a terminal before it returns. Reading a
/// poisoned lock as "no children" would skip the abort, leave the child
/// parked, and never write the return boundary.
#[tokio::test]
async fn test_join_children_survives_poisoned() {
    let entered = Arc::new(Notify::new());
    let provider = Arc::new(HangProvider {
        entered: Arc::clone(&entered),
    });
    let (runtime, store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    runtime
        .spawn(&ctx, args)
        .await
        .expect("background spawn launches");
    // The child is registered and parked in its model call; poison the
    // children lock so the join must recover it instead of reading an empty
    // close.
    entered.notified().await;
    let poisoned = catch_unwind(AssertUnwindSafe(|| {
        let _guard = runtime.children.lock().expect("children lock");
        panic!("poison the children lock");
    }));
    assert!(poisoned.is_err(), "the children lock is poisoned");
    runtime.join_children().await;
    let events = store.trajectory_tail(parent_sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "the poisoned-lock join still enumerated and finalized the parked child"
    );
}
