//! The agent tool: the model-facing entry for sub-agent delegation. The
//! tool resolves the requested type against the registry and surfaces a
//! denial distinctly from an unknown type before any spawn.
//!
//! The available-agent directory is injected into the parent system prompt as
//! a deterministic section, NOT into this description: a dynamic list embedded
//! in the tool schema busts the prompt cache on every registry change. The
//! description carries only a pointer to that section.

use std::sync::Arc;

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use serde_json::{Value, json};

use crate::agent::multi_agent::registry::{AgentError, AgentRegistry, ResolveCtx};

/// The model-facing delegation tool. Resolves the requested type against
/// the registry; never holds the runner.
pub struct AgentTool {
    registry: Arc<dyn AgentRegistry>,
}

impl AgentTool {
    pub fn new(registry: Arc<dyn AgentRegistry>) -> Self {
        Self { registry }
    }
}

const DESCRIPTION: &str = "\
Launch a new agent to delegate a sub-task. Use a specialized agent type when one fits; otherwise the general-purpose agent handles research, search, and multi-step execution.

Available agent types and their when-to-use are listed in your system context (the agent directory section), not here — a dynamic list in this description would bust the tool-schema prompt cache on every registry change.

## Writing the prompt
Brief the agent like a smart colleague who just walked into the room with no prior context:
- Explain what you're trying to accomplish and why.
- Describe what you've already learned or ruled out.
- Give enough context about the surrounding problem for the agent to act autonomously.
- State the output format you need (e.g. \"report in under 200 words\", \"return only the file paths\").
- Hand over exact commands for lookups; hand over the question for investigations.
**Never delegate understanding.** Do not write \"based on your findings, fix the bug\" — synthesize the findings yourself, then delegate the concrete fix.

## Don't peek
The tool result carries the child's summary. Do not read or tail the child's transcript mid-flight unless the user explicitly asks for a progress check — pulling the child's tool noise back into your context wastes tokens and derails the child.

## Don't race
After launching you know nothing about what the child found. Never fabricate or predict the child's results in any format before the tool result returns.

## When NOT to use
- For a single quick lookup you can do faster yourself.
- When the task needs context only you hold and cannot be conveyed in a prompt.
- To break a large task into pieces you should instead do in one turn.

Input: {description: short 3-5 word label, prompt: the task, subagent_type?: agent type (defaults to general-purpose), run_in_background?: bool (default false, blocks until the child finishes), isolation?: \"none\" | \"worktree\" (default none)}.";

impl Tool for AgentTool {
    fn name(&self) -> &str {
        "agent"
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "A short (3-5 word) description of the task"
                },
                "prompt": {
                    "type": "string",
                    "description": "The task for the agent to perform"
                },
                "subagent_type": {
                    "type": "string",
                    "description": "The type of specialized agent to use; defaults to general-purpose"
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "If true, return immediately and report completion later; if false (default), block until the child finishes"
                },
                "isolation": {
                    "type": "string",
                    "enum": ["none", "worktree"],
                    "description": "Where the child runs; \"none\" shares the parent tree, \"worktree\" uses a per-child fence. Defaults to none."
                }
            },
            "required": ["description", "prompt"]
        })
    }
    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let registry = Arc::clone(&self.registry);
        Box::pin(async move {
            // Default to the workhorse when the model omits the type, so a
            // plain delegation does not have to name a type.
            let subagent_type = input
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("general-purpose")
                .to_string();
            let resolve_ctx = ResolveCtx::default().with_denied(ctx.denied_agents.iter().cloned());
            // Resolve here for the deny check + a fast, useful error; the
            // runtime re-resolves to materialize the child (same registry).
            if let Err(e) = registry.resolve(&subagent_type, &resolve_ctx) {
                return Err(ToolError::Failed(resolve_err_msg(e)));
            }
            let prompt = input
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::InvalidInput("agent: prompt (string) required".into()))?
                .to_string();
            let summary = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or(&subagent_type)
                .to_string();
            let run_in_background = input
                .get("run_in_background")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let isolation = input
                .get("isolation")
                .and_then(|v| v.as_str())
                .unwrap_or("none")
                .to_string();
            let mut args =
                houyicoder_api::spawn::SpawnArgs::new(subagent_type.clone(), prompt, summary);
            args.isolation = isolation;
            args.run_in_background = run_in_background;
            let Some(handle) = ctx.spawn_handle.as_ref() else {
                return Err(ToolError::Failed(
                    "agent: no spawn port wired on this dispatch".into(),
                ));
            };
            match handle.spawn(&ctx, args).await {
                Ok(outcome) => Ok(build_tool_result(outcome)),
                Err(failure) => Err(ToolError::Failed(spawn_failure_msg(failure))),
            }
        })
    }
    fn is_destructive(&self) -> bool {
        false
    }
    fn is_read_only(&self) -> bool {
        // The child bears its own mutations; the parent tool only orchestrates.
        true
    }
    fn requires_approval(&self) -> bool {
        // Permission is enforced via the Agent(x) deny rule at resolve time,
        // not an Ask gate on every call.
        false
    }
}

/// Surface a resolve error as the message the model sees: an unknown type
/// lists the registered set so the model can retry; a denial says so by name.
fn resolve_err_msg(e: AgentError) -> String {
    match e {
        AgentError::NotFound {
            requested,
            available,
        } => format!(
            "agent: type {requested:?} is not registered; available types: {}",
            available.join(", "),
        ),
        AgentError::PermissionDenied { denied_type } => {
            format!("agent: type {denied_type:?} is denied by a permission rule")
        }
    }
}

/// Map a spawn rejection to a message. A budget or capability denial is
/// policy; recursion or fence failure is a wiring/depth issue; unknown agent
/// is a rare registry race the model can retry.
fn spawn_failure_msg(f: houyicoder_api::spawn::SpawnFailure) -> String {
    match f {
        houyicoder_api::spawn::SpawnFailure::BudgetExceeded => {
            "agent: spawn rejected: token budget exceeded".into()
        }
        houyicoder_api::spawn::SpawnFailure::CapabilityDenied => {
            "agent: spawn rejected: capability denied (or background spawn unsupported)".into()
        }
        houyicoder_api::spawn::SpawnFailure::Recursive => {
            "agent: spawn rejected: recursion depth cap reached".into()
        }
        houyicoder_api::spawn::SpawnFailure::FenceFail => {
            "agent: spawn rejected: worktree fence failed".into()
        }
        houyicoder_api::spawn::SpawnFailure::UnknownAgent => {
            "agent: spawn rejected: type no longer registered".into()
        }
    }
}

/// Build the tool_result JSON from a sync spawn outcome: the child's summary
/// text, its session id (the result ref for follow-up), terminal status, and
/// usage.
fn build_tool_result(outcome: houyicoder_api::spawn::SpawnOutcome) -> Value {
    let usage = outcome.usage.unwrap_or_default();
    json!({
        "status": outcome.status.unwrap_or_default(),
        "content": outcome.summary.unwrap_or_default(),
        "agentId": outcome.child_session_id,
        "result_ref": outcome.result_ref.unwrap_or_default(),
        "usage": {
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cache_read_input_tokens": usage.cache_read_input_tokens,
            "cache_write_input_tokens": usage.cache_write_input_tokens,
            "reasoning_tokens": usage.reasoning_tokens,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::SessionId;
    use std::collections::HashSet;

    fn make_tool() -> AgentTool {
        let registry = Arc::new(
            crate::agent::multi_agent::registry::BuiltInRegistry::from_agents(
                crate::agent::multi_agent::registry::built_in_all(),
            ),
        ) as Arc<dyn AgentRegistry>;
        AgentTool::new(registry)
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
        assert!(props.contains_key("run_in_background"));
        assert!(props.contains_key("isolation"));
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
        // projects it into a tool_result carrying the summary, the child
        // session id, and the usage block.
        use houyicoder_api::spawn::{SpawnArgs, SpawnFailure, SpawnHandle, SpawnOutcome};
        use houyicoder_protocol::llm::Usage;
        struct FakeSpawn;
        impl SpawnHandle for FakeSpawn {
            fn spawn(
                &self,
                _ctx: &houyicoder_api::tool::ToolCtx,
                _args: SpawnArgs,
            ) -> PFut<'_, Result<SpawnOutcome, SpawnFailure>> {
                Box::pin(async {
                    Ok(SpawnOutcome::sync(
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
            .with_spawn_handle(std::sync::Arc::new(FakeSpawn));
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
    fn test_toolctx_threads_spawn() {
        // The tool reads agent identity + spawn handle from the per-call
        // context; this pins that the port wires them through.
        use houyicoder_api::spawn::{AgentIdentity, SpawnArgs};
        struct NoSpawn;
        impl houyicoder_api::spawn::SpawnHandle for NoSpawn {
            fn spawn(
                &self,
                _ctx: &houyicoder_api::tool::ToolCtx,
                _args: SpawnArgs,
            ) -> PFut<
                '_,
                Result<houyicoder_api::spawn::SpawnOutcome, houyicoder_api::spawn::SpawnFailure>,
            > {
                Box::pin(async { Err(houyicoder_api::spawn::SpawnFailure::Recursive) })
            }
        }
        let handle: Arc<dyn houyicoder_api::spawn::SpawnHandle> = Arc::new(NoSpawn);
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
}
