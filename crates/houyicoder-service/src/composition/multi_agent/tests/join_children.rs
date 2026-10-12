//! Child-driver-completion tests: join_children stops every live child, joins
//! each detached background driver to its finalize tail, and survives a child
//! driver that panics.

use std::sync::Arc;

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::spawn::{SpawnArgs, SpawnHandle};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::SessionEvent;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, ProviderError,
};

use super::runtime_with;

/// A provider whose stream never yields, so a child running against it parks
/// inside its model call. The first entry signals the test so it waits until
/// the child has actually reached the call before joining shutdown.
struct HangProvider {
    entered: Arc<tokio::sync::Notify>,
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
    let entered = Arc::new(tokio::sync::Notify::new());
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
    let events = store.trajectory_snapshot(parent_sid);
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
    entered: Arc<tokio::sync::Notify>,
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
    let entered = Arc::new(tokio::sync::Notify::new());
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
    let events = store.trajectory_snapshot(parent_sid);
    assert!(
        events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::SubagentReturn { .. })),
        "the panicked child's finalize tail still records its return before the join resolves"
    );
}
