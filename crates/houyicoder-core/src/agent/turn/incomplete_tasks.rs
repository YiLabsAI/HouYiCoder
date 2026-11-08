//! Intercept premature final replies when unfinished tasks remain in the session.
//!
//! Models have an alignment tendency to stop after initial milestones to report
//! partial progress and ask whether to proceed. This module inspects the active
//! checklist in the durable event log and produces a continuation reminder when
//! pending or in-progress items remain, while yielding cleanly when background
//! work is in flight or when the retry cap is reached.

use std::collections::HashSet;

use houyicoder_context::{SessionEvent, SessionLogEntry};

#[cfg(test)]
mod tests;

/// Maximum consecutive continuation nudges without checklist progress or new
/// user input before allowing the turn to end.
pub(crate) const MAX_TODO_CONTINUATIONS: usize = 3;

const NUDGE_PREFIX: &str = "Note: Your task list still has ";

/// Check whether a final output should be intercepted because incomplete tasks
/// remain on the active checklist.
pub(crate) fn check_incomplete_tasks(events: &[SessionLogEntry]) -> Option<String> {
    let turn_start = events
        .iter()
        .rposition(|e| matches!(e.event, SessionEvent::UserInput { .. }))
        .unwrap_or(0);
    let turn_events = &events[turn_start..];

    if has_pending_background_work(turn_events) {
        return None;
    }

    if nudges_since_progress(turn_events) >= MAX_TODO_CONTINUATIONS {
        return None;
    }

    let incomplete = active_incomplete_todos(events, turn_events)?;
    if incomplete.is_empty() {
        return None;
    }

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
    Some(format!(
        "{NUDGE_PREFIX}{} incomplete item(s): {preview}{more}. \
         Do not stop for a mid-task status report or ask whether to continue — \
         proceed directly with the next pending item using your tools. If all \
         requested work is genuinely finished or blocked on a required user \
         decision, update the task list via todo_write first.",
        incomplete.len()
    ))
}

/// Count continuation nudges emitted since the latest user input, mid-turn
/// interjection, completion notification, or checklist update.
fn nudges_since_progress(turn_events: &[SessionLogEntry]) -> usize {
    let mut count = 0;
    for entry in turn_events.iter().rev() {
        match &entry.event {
            SessionEvent::MetaUser { text } if text.starts_with(NUDGE_PREFIX) => {
                count += 1;
            }
            SessionEvent::UserInput { .. }
            | SessionEvent::MidTurnInput { .. }
            | SessionEvent::NotificationInjected { .. } => break,
            SessionEvent::ToolResult { output, .. } if output.get("todos").is_some() => break,
            SessionEvent::ToolCall { tool, .. } if tool == "todo_write" => break,
            _ => {}
        }
    }
    count
}

/// True when the current turn launched a background command or background
/// subagent whose completion notification has not yet arrived.
fn has_pending_background_work(turn_events: &[SessionLogEntry]) -> bool {
    let notified: HashSet<&str> = turn_events
        .iter()
        .filter_map(|entry| match &entry.event {
            SessionEvent::NotificationInjected {
                child_session_id, ..
            } => Some(child_session_id.as_str()),
            _ => None,
        })
        .collect();

    for entry in turn_events {
        if let SessionEvent::ToolResult { output, .. } = &entry.event {
            if let Some(task_id) = output.get("background_task").and_then(|v| v.as_u64()) {
                let source = format!("task-{task_id}");
                if !notified.contains(source.as_str()) {
                    return true;
                }
            }
            if let Some(agent_id) = output.get("agentId").and_then(|v| v.as_str())
                && !agent_id.is_empty()
                && output.get("usage").is_none()
                && output
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .is_empty()
                && !notified.contains(agent_id)
            {
                return true;
            }
        }
    }
    false
}

/// Find the active incomplete todo items for this turn. Prefers a checklist
/// written inside the current user turn; falls back to an earlier checklist
/// only when the current turn has already executed tool calls.
fn active_incomplete_todos(
    all_events: &[SessionLogEntry],
    turn_events: &[SessionLogEntry],
) -> Option<Vec<String>> {
    if let Some(items) = latest_incomplete_todos(turn_events) {
        return Some(items);
    }
    let has_tool_activity = turn_events
        .iter()
        .any(|e| matches!(e.event, SessionEvent::ToolCall { .. }));
    if has_tool_activity {
        return latest_incomplete_todos(all_events);
    }
    None
}

/// Scan events in reverse for the newest todo_write and return descriptions of
/// items whose status is pending or in_progress.
fn latest_incomplete_todos(events: &[SessionLogEntry]) -> Option<Vec<String>> {
    for entry in events.iter().rev() {
        match &entry.event {
            SessionEvent::ToolResult { output, .. } => {
                if let Some(todos) = output.get("todos").and_then(|v| v.as_array()) {
                    return Some(collect_incomplete(todos));
                }
            }
            SessionEvent::ToolCall { tool, input, .. } if tool == "todo_write" => {
                if let Some(todos) = input.get("todos").and_then(|v| v.as_array()) {
                    return Some(collect_incomplete(todos));
                }
            }
            _ => {}
        }
    }
    None
}

fn collect_incomplete(todos: &[serde_json::Value]) -> Vec<String> {
    let mut incomplete = Vec::new();
    for item in todos {
        let status = item.get("status").and_then(|s| s.as_str());
        let content = item.get("content").and_then(|c| c.as_str()).unwrap_or("");
        if matches!(status, Some("pending" | "in_progress")) {
            incomplete.push(content.to_string());
        }
    }
    incomplete
}
