//! Direct shell: one user-typed command executed outside a model turn.
//!
//! Submitting the command is the single-action authorization, so it runs
//! through the registered bash tool a model-issued call uses, keeping the
//! sandbox fence, capture, hook gate, and cancellation race in one place. No
//! model request is made; the command and its result append to the session
//! log. A command that dies between the two leaves a ToolCall with no result,
//! which the next model run reconciles before appending its own input.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::atomic::Ordering;

use houyicoder_context::EventId;
use houyicoder_context::SessionId;
use houyicoder_protocol::extension::ToolError;
use houyicoder_protocol::tool::user_shell_call_id;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::agent::RunError;
use crate::agent::Runner;
use crate::agent::turn::fallback::tool_error_json;
use crate::agent::turn::resolve::ToolCallPlan;

/// The registered tool a direct shell command runs through. The input is shell
/// text, so the bash tool owns its execution and its output capture.
const DIRECT_SHELL_TOOL: &str = "bash";

/// Why a direct shell command was refused before it could run. A command that
/// runs and fails is not one of these: its failure is the output the tool
/// produced, which the caller answers like any other result.
#[derive(Debug)]
pub enum DirectShellError {
    /// The command holds no text to run.
    EmptyCommand,
    /// The tool registry holds no tool that runs shell text.
    NoShellTool,
    /// A model turn is paused on an approval decision, and that wait owns the
    /// cancel slot a command would take.
    RunnerPaused,
    /// A model turn or another command is running. Taking the cancel slot now
    /// would leave that call unstoppable, so the command waits for it.
    Busy,
    /// The session log could not keep the call or its result.
    Transcript(RunError),
}

impl fmt::Display for DirectShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCommand => write!(f, "direct shell: the command is empty"),
            Self::NoShellTool => {
                write!(f, "direct shell: no {DIRECT_SHELL_TOOL} tool is registered")
            }
            Self::RunnerPaused => {
                write!(
                    f,
                    "direct shell: the run is waiting for an approval decision"
                )
            }
            Self::Busy => write!(
                f,
                "direct shell: another run or command is still in progress"
            ),
            Self::Transcript(error) => write!(f, "direct shell transcript write failed: {error}"),
        }
    }
}

impl Error for DirectShellError {}

impl Runner {
    /// Run one user-typed command in the session sandbox and answer the bounded
    /// result the tool produced. No model request is made.
    ///
    /// The command passes the same PreToolUse gate and required-argument check
    /// a model-issued call passes, then runs through the authorized entry
    /// point: submitting the command is the approval, while a gate verdict that
    /// denies still refuses it. A tool failure becomes the call's own result.
    /// Only the conditions in DirectShellError are reported to the caller, so
    /// a caller telling a refusal from a failed command reads the type instead
    /// of the message.
    pub async fn run_direct_shell(
        &self,
        session: SessionId,
        command: String,
    ) -> Result<serde_json::Value, DirectShellError> {
        // The terminal refuses a blank submission, so this guard is what keeps
        // any other client from recording a call whose only content is blank
        // text.
        if command.trim().is_empty() {
            return Err(DirectShellError::EmptyCommand);
        }
        // Held for the whole command: a model run that starts while it runs,
        // or a second command, would otherwise take the cancel slot.
        let Some(_drive) = self.begin_drive() else {
            return Err(DirectShellError::Busy);
        };
        let Some(tool) = self.tools().get(DIRECT_SHELL_TOOL) else {
            return Err(DirectShellError::NoShellTool);
        };
        // A paused run owns the cancel slot and the durable abort flag; an
        // abort issued here would be read as its answer and skip its resume.
        if self.paused.load(Ordering::Acquire) {
            return Err(DirectShellError::RunnerPaused);
        }
        // The id joins the call to its result, keys the approval scan, and
        // marks the call as user-typed for every reader of the log, so it stays
        // unique across restarts where a per-process counter would not.
        let call_id = user_shell_call_id(&EventId::new().to_string());
        let typed = json!({ "command": command });
        let mut plans = vec![ToolCallPlan::new(
            call_id.clone(),
            tool.clone(),
            typed.clone(),
        )];
        // In place before the first await, so a cancel that lands while the
        // call is being appended still reaches the execution race.
        let token = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex") = Some(token.clone());
        tracing::debug!(call_id = %call_id, "direct shell command starting");
        self.append_tool_call(session, &call_id, DIRECT_SHELL_TOOL, typed)
            .await
            .map_err(DirectShellError::Transcript)?;
        let blocked = self.run_pre_tool_use_gate(session, &mut plans).await;
        let output = match blocked.into_iter().next() {
            Some((_, refusal)) => {
                let output = refusal;
                self.append_tool_result(
                    session,
                    call_id.clone(),
                    DIRECT_SHELL_TOOL,
                    output.clone(),
                    0,
                )
                .await
                .map_err(DirectShellError::Transcript)?;
                output
            }
            None => {
                // The gate removes a call only by answering it above, so a call
                // that reaches this point is the one still queued.
                let plan = plans.pop().expect("the hook gate keeps a call it allows");
                if plan.invalid_arguments.is_empty() {
                    self.execute_approved_call(session, &plan.id, &plan.tool, plan.input, &token)
                        .await
                        .map_err(DirectShellError::Transcript)?
                } else {
                    // The answer the model path gives such a call, so an empty
                    // or partial command is refused rather than run.
                    let message = format!(
                        "{DIRECT_SHELL_TOOL}: {} required",
                        plan.invalid_arguments.join(", ")
                    );
                    let output = tool_error_json(&ToolError::InvalidInput(message));
                    self.append_tool_result(
                        session,
                        call_id.clone(),
                        DIRECT_SHELL_TOOL,
                        output.clone(),
                        0,
                    )
                    .await
                    .map_err(DirectShellError::Transcript)?;
                    output
                }
            }
        };
        let names = HashMap::from([(call_id.clone(), DIRECT_SHELL_TOOL.to_string())]);
        self.fold_tool_outcomes(&[(call_id, output.clone())], &names);
        Ok(output)
    }
}

#[cfg(test)]
mod tests;
