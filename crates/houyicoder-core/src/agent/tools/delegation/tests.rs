//! Tests for this module.

use super::*;
use houyicoder_context::SessionId;
use std::collections::HashSet;

fn make_tool() -> DelegationTool {
    let registry = Arc::new(BuiltInRegistry::from_agents(built_in_all())) as Arc<dyn AgentRegistry>;
    DelegationTool::new(registry)
}

fn ctx_with_denied(denied: &[&str]) -> ToolCtx {
    let set: HashSet<String> = denied.iter().map(|s| s.to_string()).collect();
    ToolCtx::new("call-1").with_denied_agents(Arc::new(set))
}

#[test]
fn test_name_is_agent() {
    assert_eq!(make_tool().name(), "agent");
}

#[test]
fn test_schema_has_fields() {
    let schema = make_tool().input_schema();
    let props = schema.get("properties").unwrap().as_object().unwrap();
    assert!(props.contains_key("description"));
    assert!(props.contains_key("prompt"));
    assert!(props.contains_key("subagent_type"));
    assert!(props.contains_key("isolation"));
    // Background execution remains hidden until the TUI can stop a child.
    assert!(
        !props.contains_key("run_in_background"),
        "run_in_background must stay out of the schema until kill UX lands"
    );
    let required = schema.get("required").unwrap().as_array().unwrap();
    assert!(required.iter().any(|v| v == "description"));
    assert!(required.iter().any(|v| v == "prompt"));
}

#[test]
fn test_description_carries_briefing() {
    let t = make_tool();
    let d = t.description();
    assert!(d.contains("Writing the prompt"));
    assert!(d.contains("Never delegate understanding"));
    // Delegation-hygiene guidance
    assert!(d.contains("Don't peek"));
    assert!(d.contains("Don't race"));
    // The directory lives in system context, not the description
    assert!(d.contains("system context"));
}

#[test]
fn test_description_omits_names() {
    // A list embedded here would bust the tool-schema prompt cache on
    // every registry change; the directory is injected into the system
    // prompt instead.
    let t = make_tool();
    let d = t.description();
    assert!(!d.contains("(Tools:"));
    assert!(!d.contains("- explore:") && !d.contains("- plan:"));
}

#[test]
fn test_traits_read_only_ungated() {
    let t = make_tool();
    assert!(t.is_read_only());
    assert!(!t.is_destructive());
    assert!(!t.requires_approval());
}

#[test]
fn test_resolve_defaults_to_general() {
    // Omitting subagent_type resolves to the workhorse, not an error.
    let t = make_tool();
    let out = pollster::block_on(t.execute(
        ToolCtx::new("c1"),
        json!({"description": "x", "prompt": "y"}),
    ));
    let msg = out.unwrap_err().to_string();
    // Resolved fine; the failure is the not-yet-wired spawn path.
    assert!(!msg.contains("is not registered"), "{msg}");
}

#[test]
fn test_resolve_unknown_lists_available() {
    let t = make_tool();
    let out = pollster::block_on(t.execute(
        ToolCtx::new("c1"),
        json!({"description": "x", "prompt": "y", "subagent_type": "no-such"}),
    ));
    let msg = out.unwrap_err().to_string();
    assert!(msg.contains("is not registered"), "{msg}");
    assert!(
        msg.contains("explore"),
        "available list must name built-ins: {msg}"
    );
    assert!(msg.contains("plan"), "{msg}");
}

#[test]
fn test_resolve_deny_surfaces_distinctly() {
    // A denied type resolves as a denial, not as unknown — the model
    // must see the cause is policy, not a typo.
    let t = make_tool();
    let out = pollster::block_on(t.execute(
        ctx_with_denied(&["explore"]),
        json!({"description": "x", "prompt": "y", "subagent_type": "explore"}),
    ));
    let msg = out.unwrap_err().to_string();
    assert!(msg.contains("denied"), "{msg}");
    assert!(!msg.contains("is not registered"), "{msg}");
}

#[test]
fn test_execute_builds_tool_result() {
    // A spawn handle that returns a canned terminal outcome: the tool
    // maps it into a tool_result carrying the summary, the child
    // session id, and the usage block.
    struct FakeSpawn;
    impl SpawnHandle for FakeSpawn {
        fn spawn(
            &self,
            _ctx: &ToolCtx,
            _args: SpawnArgs,
        ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
            Box::pin(async {
                Ok(SpawnOutcome::foreground(
                    "child-sid",
                    "completed",
                    "the child answer",
                    Usage {
                        input_tokens: 100,
                        output_tokens: 20,
                        ..Usage::default()
                    },
                ))
            })
        }
    }
    let t = make_tool();
    let ctx = ToolCtx::new("c1")
        .with_session(SessionId::new())
        .with_spawn_handle(Arc::new(FakeSpawn));
    let out = pollster::block_on(t.execute(
        ctx,
        json!({"description": "find auth", "prompt": "find the auth module"}),
    ))
    .expect("execute should succeed");
    assert_eq!(out["status"], "completed");
    assert_eq!(out["content"], "the child answer");
    assert_eq!(out["agentId"], "child-sid");
    assert_eq!(out["usage"]["input_tokens"], 100);
    assert_eq!(out["usage"]["output_tokens"], 20);
}

#[test]
fn test_tool_result_placeholder() {
    let out = build_tool_result(
        SpawnOutcome::foreground("c1", "completed", "", Usage::default()),
        None,
    );
    assert_eq!(out["content"], "(Subagent returned no output.)");
    // No color surfaces as JSON null, not omitted.
    assert!(out["color"].is_null());
}

/// A background outcome reports that the child started without presenting
/// an empty foreground result.
#[test]
fn test_background_result_reports_start() {
    let out = build_tool_result(SpawnOutcome::background_started("c1"), None);
    assert_eq!(
        out["status"], "",
        "background outcome carries no terminal status"
    );
    let content = out["content"].as_str().unwrap_or("");
    assert!(
        content.contains("Launched in the background"),
        "background outcome surfaces a launched message, not 'returned no output': {content}"
    );
    assert!(
        !content.contains("returned no output"),
        "background must not use the foreground empty-result placeholder: {content}"
    );
}

#[test]
fn test_tool_result_carries_color() {
    let out = build_tool_result(
        SpawnOutcome::foreground("c1", "completed", "done", Usage::default()),
        Some("red"),
    );
    assert_eq!(out["color"], "red");
}

#[test]
fn test_tool_result_truncates_tail() {
    // 100k + 5 chars: the tail keeps the last 100k; the marker notes the
    // child log holds the full output.
    let big: String = "a".repeat(100_005);
    let out = build_tool_result(
        SpawnOutcome::foreground("child-xyz", "completed", &big, Usage::default()),
        None,
    );
    let content = out["content"].as_str().unwrap();
    assert!(content.starts_with("[truncated; the child log holds the full output]"));
    // tail-preserving: the last chars of the original survive.
    assert!(content.ends_with("aaaaa"));
    assert!(content.chars().count() < big.chars().count() + 200);
}

#[test]
fn test_toolctx_threads_spawn() {
    // The per-call context carries agent identity and the spawn handle.
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
    let handle: Arc<dyn SpawnHandle> = Arc::new(NoSpawn);
    let identity = AgentIdentity {
        subagent_type: Some("explore".into()),
        depth: 0,
        parent_session_id: None,
    };
    let ctx = ToolCtx::new("c1")
        .with_agent_identity(identity)
        .with_spawn_handle(handle);
    assert_eq!(ctx.agent_identity.as_ref().unwrap().depth, 0);
    assert!(ctx.spawn_handle.is_some());
    let _sid = SessionId::new();
}

/// A boundary the store could not write reaches the model as that, not as
/// a worktree failure: the two have different fixes, and a false cause
/// sends the model to retry with isolation off, which fails the same way.
#[test]
fn test_boundary_failure_message() {
    let msg = spawn_failure_msg(SpawnFailure::BoundaryWriteFailed);
    assert!(
        msg.contains("delegation could not be recorded"),
        "the refusal names what could not be recorded: {msg}"
    );
    assert!(
        !msg.contains("worktree"),
        "a boundary write failure is not a worktree failure: {msg}"
    );
}
