//! Shutdown-versus-spawn races: the completion gate must hold every child
//! the parent created, including ones whose driver has not registered yet,
//! and a stop must reach a child before its first poll.

use std::sync::Arc;

use houyicoder_api::provider::ModelProvider;
use houyicoder_api::spawn::{SpawnArgs, SpawnHandle};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::{PFut, PStream};
use houyicoder_context::SessionEvent;
use houyicoder_core::agent::ToolRegistry;
use houyicoder_protocol::llm::{
    CompletionRequest, CompletionResponse, LlmEvent, ModelCapabilities, OutputItem, ProviderError,
    Usage,
};

use super::runtime_with;

/// A provider that parks the child in its model call and only returns once
/// the test releases it, so the finalize tail is observably late.
struct GateProvider {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
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
        Box::pin(futures::stream::once(async move {
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
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
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
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
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
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
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
