//! Intercept premature final replies when unfinished tasks remain in the session.
//!
//! Models have an alignment tendency to stop after initial milestones to report
//! partial progress and ask whether to proceed. This module detects incomplete
//! items on the active checklist and produces a continuation reminder so the
//! driver loop can keep working.

use houyicoder_context::{SessionEvent, SessionLogEntry};

#[cfg(test)]
mod tests;

/// Check whether a final output should be intercepted because incomplete tasks
/// remain on the active checklist, or because the user requested continuous
/// execution and the model paused for confirmation.
pub(crate) fn check_incomplete_tasks(
    events: &[SessionLogEntry],
    final_text: &str,
) -> Option<String> {
    if let Some(incomplete) = latest_incomplete_todos(events)
        && !incomplete.is_empty()
    {
        let preview = incomplete
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let more = if incomplete.len() > 3 {
            format!(" and {} more", incomplete.len() - 3)
        } else {
            String::new()
        };
        return Some(format!(
            "Note: Your task list still has {} incomplete item(s): {preview}{more}. \
             Do not stop for a mid-task status report or ask whether to continue — \
             proceed directly with the next pending item using your tools. If all \
             requested work is genuinely finished or blocked on a required user \
             decision, update the task list via todo_write first.",
            incomplete.len()
        ));
    }

    if let Some(user_text) = latest_user_input_text(events) {
        let lower_user = user_text.to_lowercase();
        let wants_continuous = lower_user.contains("不要停")
            || lower_user.contains("一次性")
            || lower_user.contains("一口气")
            || lower_user.contains("全部完成")
            || lower_user.contains("without stopping")
            || lower_user.contains("end-to-end");
        if wants_continuous {
            let lower_final = final_text.to_lowercase();
            let asks_confirmation = lower_final.contains("要我接着")
                || lower_final.contains("要继续吗")
                || lower_final.contains("是否继续")
                || lower_final.contains("要接着")
                || lower_final.contains("shall i continue")
                || lower_final.contains("would you like me to continue");
            if asks_confirmation {
                return Some(
                    "Note: You were instructed to complete the tasks end-to-end \
                     without stopping. Do not pause to ask whether to continue — proceed \
                     directly with the remaining tasks using your tools."
                        .to_string(),
                );
            }
        }
    }

    None
}

/// Find the latest todo_write in the event log and return the descriptions of
/// items whose status is pending or in_progress. Returns None when no
/// todo_write was ever recorded.
fn latest_incomplete_todos(events: &[SessionLogEntry]) -> Option<Vec<String>> {
    for entry in events.iter().rev() {
        match &entry.event {
            SessionEvent::ToolResult { output, .. } => {
                if let Some(todos) = output.get("todos").and_then(|v| v.as_array()) {
                    let mut incomplete = Vec::new();
                    for item in todos {
                        let status = item.get("status").and_then(|s| s.as_str());
                        let content = item.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        if matches!(status, Some("pending" | "in_progress")) {
                            incomplete.push(content.to_string());
                        }
                    }
                    return Some(incomplete);
                }
            }
            SessionEvent::ToolCall { tool, input, .. } if tool == "todo_write" => {
                if let Some(todos) = input.get("todos").and_then(|v| v.as_array()) {
                    let mut incomplete = Vec::new();
                    for item in todos {
                        let status = item.get("status").and_then(|s| s.as_str());
                        let content = item.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        if matches!(status, Some("pending" | "in_progress")) {
                            incomplete.push(content.to_string());
                        }
                    }
                    return Some(incomplete);
                }
            }
            _ => {}
        }
    }
    None
}

/// Read the newest user-input text from the session events.
fn latest_user_input_text(events: &[SessionLogEntry]) -> Option<&str> {
    events.iter().rev().find_map(|entry| match &entry.event {
        SessionEvent::UserInput { text } => Some(text.as_str()),
        _ => None,
    })
}
