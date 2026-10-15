//! Tests for this module.

use std::sync::Arc;

use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnOutcome};
use houyicoder_api::tool::ToolCtx;
use houyicoder_async::PFut;

use super::*;
use crate::agent::ToolRegistry;
use crate::agent::tests::runner_with;
use crate::provider::test_support::FakeProvider;

/// A handle that accepts nothing, but enough of a wired port that a join
/// delegated through it resolves instead of reaching for a real runtime.
struct NoSpawn;
impl SpawnHandle for NoSpawn {
    fn spawn(
        &self,
        _ctx: &ToolCtx,
        _args: SpawnArgs,
    ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
        Box::pin(async { Err(SpawnFailure::Recursive) })
    }
}

fn runner() -> Runner {
    runner_with(Arc::new(FakeProvider::text("ok")), ToolRegistry::new())
}

/// Without a spawn handle there is nothing to stop or join, so this
/// resolves immediately instead of erroring or hanging the host teardown.
#[tokio::test]
async fn test_join_children_without_handle() {
    runner().join_children().await;
}

/// A runner carrying a handle delegates the join, so the host reaches the
/// real runtime's child finalization rather than short-circuiting it.
#[tokio::test]
async fn test_join_children_delegates() {
    let runner = runner().with_spawn_handle(Arc::new(NoSpawn));
    runner.join_children().await;
}
