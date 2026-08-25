//! Hook event descriptions for the /hooks detail view. Split from hook.rs
//! on the file-size gate.

use super::HookEvent;

impl HookEvent {
    /// Full description shown in the /hooks detail view: what JSON input the
    /// hook command receives and what verdict to return for what effect.
    pub fn description(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => {
                "Input is JSON with tool_name and tool_input. Return Deny to block the call, Feedback to inject a correction, or Allow to proceed."
            }
            HookEvent::PostToolUse => {
                "Input is JSON with tool_name, tool_input, and tool_response. Return Feedback to inject a correction, or Allow to proceed. Observability only for most cases."
            }
            HookEvent::PostToolUseFailure => {
                "Input is JSON with tool_name, tool_input, and error. Observability only; the verdict is not enforced."
            }
            HookEvent::SessionStart => {
                "Input is JSON with source and session_id. Observability only."
            }
            HookEvent::SessionEnd => "Input is JSON with session_id. Observability only.",
            HookEvent::Setup => "Input is JSON with repo_path. Observability only.",
            HookEvent::UserPromptSubmit => {
                "Input is JSON with the user prompt text. Return Deny to block, Feedback to inject, or Allow to proceed."
            }
            HookEvent::Stop => {
                "Input is JSON with the agent response. Return Feedback to continue the turn, or Allow to end."
            }
            HookEvent::StopFailure => "Input is JSON with the error. Observability only.",
            HookEvent::Notification => "Input is JSON with the notification. Observability only.",
            HookEvent::PreCompact => {
                "Input is JSON with the conversation summary context. Return Inject to add instructions to the summarizer, or Allow to proceed."
            }
            HookEvent::PostCompact => {
                "Input is JSON with the summary and folded turn count. Observability only."
            }
            HookEvent::PreSelect => {
                "Input is JSON with the context window state. Return Inject to add context, or Allow to proceed."
            }
            HookEvent::InstructionsLoaded => {
                "Input is JSON with file_path, memory_type, and load_reason. Observability only; does not support blocking."
            }
            HookEvent::CwdChanged => "Input is JSON with old_cwd and new_cwd. Observability only.",
            HookEvent::FileChanged => {
                "Input is JSON with file_path and event type. Observability only."
            }
            HookEvent::ConfigChange => {
                "Input is JSON with source and file_path. Return Deny to block the change, or Allow to proceed."
            }
            HookEvent::SubagentStart => {
                "Input is JSON with the subagent type and prompt. Observability only."
            }
            HookEvent::SubagentStop => {
                "Input is JSON with the subagent result. Observability only."
            }
            HookEvent::PermissionRequest => {
                "Input is JSON with the tool call and requested permission. Return Deny to refuse, or Allow to grant."
            }
            HookEvent::PermissionDenied => {
                "Input is JSON with the denied tool call. Observability only."
            }
            HookEvent::TeammateIdle => "Input is JSON with the teammate state. Observability only.",
            HookEvent::TaskCreated => {
                "Input is JSON with the task description. Observability only."
            }
            HookEvent::TaskCompleted => "Input is JSON with the task result. Observability only.",
            HookEvent::Elicitation => {
                "Input is JSON with the MCP server request. Observability only."
            }
            HookEvent::ElicitationResult => {
                "Input is JSON with the user response. Observability only."
            }
            HookEvent::WorktreeCreate => {
                "Input is JSON with the worktree name. Return the worktree path on stdout, or Deny to block."
            }
            HookEvent::WorktreeRemove => {
                "Input is JSON with the worktree path. Return Allow to proceed, or Deny to block."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HookEvent;

    #[test]
    fn test_all_events_have_description() {
        for e in HookEvent::ALL {
            let d = e.description();
            assert!(!d.is_empty(), "{e:?} has empty description");
            assert!(
                d.contains("Input is JSON"),
                "{e:?} description should name the input format"
            );
        }
    }

    #[test]
    fn test_blocking_events_mention_deny() {
        for e in [
            HookEvent::PreToolUse,
            HookEvent::UserPromptSubmit,
            HookEvent::ConfigChange,
        ] {
            assert!(
                e.description().contains("Deny"),
                "{e:?} should mention Deny in its description"
            );
        }
    }

    #[test]
    fn test_observability_only_events() {
        for e in [
            HookEvent::PostToolUseFailure,
            HookEvent::Notification,
            HookEvent::SessionEnd,
        ] {
            assert!(
                e.description().contains("Observability only"),
                "{e:?} should be observability only"
            );
        }
    }
}
