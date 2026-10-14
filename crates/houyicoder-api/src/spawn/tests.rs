//! Tests for this module.

use super::*;

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

#[tokio::test]
async fn test_spawn_handle_callable() {
    let handle: Box<dyn SpawnHandle> = Box::new(NoSpawn);
    let args = SpawnArgs::new("explore", "find auth", "find auth");
    assert_eq!(args.isolation, "none");
    assert!(!args.run_in_background);
    let ctx = ToolCtx::new("c1");
    let outcome = handle.spawn(&ctx, args).await;
    assert!(matches!(outcome, Err(SpawnFailure::Recursive)));
}

/// The default spawn_system returns Err (a runtime that does not expose
/// first-party spawn refuses, so a service/hook caller learns it is
/// unsupported rather than panicking).
#[tokio::test]
async fn test_spawn_system_default_refuses() {
    let handle: Box<dyn SpawnHandle> = Box::new(NoSpawn);
    let args = SpawnArgs::new("explore", "review", "review");
    let outcome = handle
        .spawn_system(SessionId::new(), "review_gate", args)
        .await;
    assert!(
        matches!(outcome, Err(SpawnFailure::Recursive)),
        "default spawn_system refuses, got {outcome:?}"
    );
}

/// The default join_children resolves immediately: a handle with no
/// multi-agent runtime wired has no children to stop or drivers to join.
#[tokio::test]
async fn test_join_children_default_resolves() {
    let handle: Box<dyn SpawnHandle> = Box::new(NoSpawn);
    handle.join_children().await;
}

#[test]
fn test_toolctx_carries_spawn_context() {
    let identity = AgentIdentity {
        subagent_type: Some("explore".to_string()),
        depth: 1,
        parent_session_id: Some("parent-sid".to_string()),
    };
    let handle: Arc<dyn SpawnHandle> = Arc::new(NoSpawn);
    let ctx = ToolCtx::new("call-1")
        .with_agent_identity(identity)
        .with_spawn_handle(handle);
    assert_eq!(ctx.agent_identity.as_ref().unwrap().depth, 1);
    assert_eq!(
        ctx.agent_identity
            .as_ref()
            .unwrap()
            .subagent_type
            .as_deref(),
        Some("explore")
    );
    assert!(ctx.spawn_handle.is_some());
    assert!(ToolCtx::new("call-2").agent_identity.is_none());
    assert!(ToolCtx::new("call-3").spawn_handle.is_none());
}
