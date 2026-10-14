//! The delegation tool: the model-facing entry for sub-agent delegation.
//! The tool resolves the requested type against the registry and surfaces a
//! denial distinctly from an unknown type before any spawn.
//!
//! The available-agent directory is injected into the parent system prompt as
//! a deterministic section, NOT into this description: a dynamic list embedded
//! in the tool schema busts the prompt cache on every registry change. The
//! description carries only a pointer to that section.

use std::sync::Arc;

use houyicoder_api::spawn::SpawnFailure;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use serde_json::{Value, json};

#[cfg(test)]
use crate::agent::multi_agent::registry::BuiltInRegistry;
#[cfg(test)]
use crate::agent::multi_agent::registry::built_in_all;
use crate::agent::multi_agent::registry::{AgentError, AgentRegistry, ResolveCtx};
#[cfg(test)]
use houyicoder_api::spawn::AgentIdentity;
use houyicoder_api::spawn::SpawnArgs;
#[cfg(test)]
use houyicoder_api::spawn::SpawnHandle;
use houyicoder_api::spawn::SpawnOutcome;
#[cfg(test)]
use houyicoder_protocol::llm::Usage;

/// The model-facing delegation tool. Resolves the requested type against
/// the registry; never holds the runner.
pub struct DelegationTool {
    registry: Arc<dyn AgentRegistry>,
}

impl DelegationTool {
    pub fn new(registry: Arc<dyn AgentRegistry>) -> Self {
        Self { registry }
    }
}

/// The tool name a model delegation is issued through. Shared with the fold
/// that reads the child usage back out of its result, so a rename cannot leave
/// one side matching a name the other no longer uses.
pub(crate) const NAME: &str = "agent";

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

Input: {description: short 3-5 word label, prompt: the task, subagent_type?: agent type (defaults to general-purpose), isolation?: \"none\" | \"worktree\" (default none)}.

Subagents run in the foreground, so the parent waits for the child result. Background delegation remains hidden until the TUI can stop a running child.";

impl Tool for DelegationTool {
    fn name(&self) -> &str {
        NAME
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
                // run_in_background is intentionally absent from the schema:
                // a background child cannot be stopped from the TUI yet, so
                // exposing it would let an unkillable agent run. The wire
                // struct still carries the field (defaults false) for when
                // the kill UX lands.
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
            // The resolved def also carries the badge color surfaced in the
            // tool result so the TUI can color the teammate header + fold
            // summary without a second lookup.
            let def = registry
                .resolve(&subagent_type, &resolve_ctx)
                .map_err(|e| ToolError::Failed(resolve_err_msg(e)))?;
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
            let mut args = SpawnArgs::new(subagent_type.clone(), prompt, summary);
            args.isolation = isolation;
            args.run_in_background = run_in_background;
            let Some(handle) = ctx.spawn_handle.as_ref() else {
                return Err(ToolError::Failed(
                    "agent: no spawn port wired on this dispatch".into(),
                ));
            };
            match handle.spawn(&ctx, args).await {
                Ok(outcome) => Ok(build_tool_result(outcome, def.color.as_deref())),
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
/// policy; recursion or fence failure is a wiring/depth issue; a boundary the
/// store refused is an unwritable session directory; unknown agent is a rare
/// registry race the model can retry.
fn spawn_failure_msg(f: SpawnFailure) -> String {
    match f {
        SpawnFailure::BudgetExceeded => "agent: spawn rejected: token budget exceeded".into(),
        SpawnFailure::CapabilityDenied => {
            "agent: spawn rejected: capability denied (or background spawn unsupported)".into()
        }
        SpawnFailure::Recursive => "agent: spawn rejected: recursion depth cap reached".into(),
        SpawnFailure::FenceFail => "agent: spawn rejected: worktree fence failed".into(),
        // The boundary that can fail holds three records: the child's own, the
        // parent's spawn, and the parent's return. Naming one of them would
        // name a file rather than the cause, and all three have the same fix.
        SpawnFailure::BoundaryWriteFailed => {
            "agent: spawn rejected: the delegation could not be recorded".into()
        }
        SpawnFailure::UnknownAgent => "agent: spawn rejected: type no longer registered".into(),
        SpawnFailure::ConcurrencySaturated => {
            "agent: spawn rejected: concurrent-spawn cap saturated, retry next turn".into()
        }
        SpawnFailure::ShuttingDown => {
            "agent: spawn rejected: the session host is shutting down".into()
        }
        SpawnFailure::ConstructionFailed => {
            "agent: spawn rejected: the child failed to start".into()
        }
    }
}

/// Build the tool_result JSON from a spawn outcome. A sync outcome carries
/// the child's summary text, session id (the result ref for follow-up),
/// terminal status, and usage; an empty summary gets a placeholder so the
/// parent does not read "nothing" as "done", and a summary past the 100k
/// char cap is tail-truncated with a note that the child log holds the full
/// output. A background outcome (no terminal status) surfaces a "launched in
/// the background" message instead — the result arrives later as a
/// turn-boundary notification, not as this tool_result.
fn build_tool_result(outcome: SpawnOutcome, color: Option<&str>) -> Value {
    let usage = outcome.usage.unwrap_or_default();
    // A background outcome carries no terminal status or summary — the parent
    // does not block on the child. Surface that distinctly so the model does
    // not read "returned no output" (a foreground empty-result symptom) and
    // instead knows the result arrives later as a turn-boundary notification.
    let background_started = outcome.status.is_none();
    let status = outcome.status.clone().unwrap_or_default();
    let child = outcome.child_session_id.clone();
    let result_ref = outcome.result_ref.unwrap_or_default();
    let mut content = outcome.summary.unwrap_or_default();
    if background_started {
        content = "Launched in the background; the result arrives as a \
                   notification on your next turn."
            .into();
    } else if content.is_empty() {
        content = "(Subagent returned no output.)".into();
    }
    const MAX_CHARS: usize = 100_000;
    let truncated = content.chars().count() > MAX_CHARS;
    if truncated {
        let tail: String = content
            .chars()
            .rev()
            .take(MAX_CHARS)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        content = format!("[truncated; the child log holds the full output]\n{tail}",);
    }
    // The badge color is surfaced so the TUI can color the teammate header
    // and the inline fold-group summary without a registry lookup. Null
    // (not omitted) when the agent has no color, so the TUI distinguishes
    // unset from a default.
    let color = match color {
        Some(c) => Value::String(c.to_string()),
        None => Value::Null,
    };
    json!({
        "status": status,
        "content": content,
        "agentId": child,
        // A future retrieval seam: a pointer the model could use to fetch the
        // child transcript without re-running the child. No reader exists
        // today, so it is not a live path — kept on the envelope so a later
        // retrieval tool can wire it without a wire change.
        "result_ref": result_ref,
        "color": color,
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
mod tests;
