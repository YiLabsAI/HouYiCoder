//! The fleet queries the model-facing task tools read: the list of children
//! the parent delegates to, and one child's output through a character
//! window. Both fold the durable parent log as the source of truth — the
//! spawn boundary carries the type, description, and launch mode — and mark
//! a child running when the runtime's table still holds it.

use houyicoder_api::spawn::{ChildOutputView, ChildTaskSummary};
use houyicoder_context::{SessionEvent, SessionId};
use std::collections::HashMap;

use super::MultiAgentRuntime;

impl MultiAgentRuntime {
    /// List every child the parent session delegates to, running and
    /// finished. The parent log's spawn boundaries name every child; the
    /// runtime's table marks which are still live.
    pub(super) async fn fleet_list(&self, parent_sid: SessionId) -> Vec<ChildTaskSummary> {
        let Ok(log) = self.store.replay(parent_sid).await else {
            return Vec::new();
        };
        // A child whose entry still holds a live runner is running; a dead
        // Weak leaves the key behind but the child is finished, so its
        // durable state stands.
        let running: Vec<String> = self
            .children
            .lock()
            .map(|kids| {
                kids.iter()
                    .filter(|(_, e)| e.runner.upgrade().is_some())
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut rows: Vec<ChildTaskSummary> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for entry in log {
            match &entry.event {
                SessionEvent::SubagentSpawn {
                    child_session_id,
                    subagent_type,
                    prompt_summary,
                    ..
                } => {
                    match index.get(child_session_id.as_str()) {
                        // A second spawn for the same child id is a resume: the
                        // child is delegated to again, so the row resets to the
                        // new description and a cleared state rather than a
                        // duplicate listing the same id twice.
                        Some(at) => {
                            rows[*at].description = prompt_summary.clone();
                            rows[*at].subagent_type = subagent_type.clone();
                            rows[*at].status = String::new();
                        }
                        None => {
                            let row = ChildTaskSummary {
                                child_session_id: child_session_id.clone(),
                                subagent_type: subagent_type.clone(),
                                description: prompt_summary.clone(),
                                status: String::new(),
                                turn_count: 0,
                                elapsed_ms: 0,
                            };
                            index.insert(child_session_id.clone(), rows.len());
                            rows.push(row);
                        }
                    }
                }
                SessionEvent::SubagentReturn {
                    child_session_id,
                    status,
                    duration_ms,
                    tool_calls,
                    ..
                } => {
                    if let Some(at) = index.get(child_session_id.as_str()) {
                        rows[*at].status = status.clone();
                        rows[*at].elapsed_ms = *duration_ms;
                        // The return carries the child's own tool-call count,
                        // the closest durable measure of the work it did.
                        rows[*at].turn_count = u64::from(*tool_calls);
                    }
                }
                _ => {}
            }
        }
        // A child with no return on record and no live entry reads as open:
        // the durable boundary says the delegation has not ended.
        for row in rows.iter_mut() {
            if row.status.is_empty() {
                let live = running.contains(&row.child_session_id);
                row.status = if live {
                    "running".into()
                } else {
                    "open".into()
                };
            }
        }
        rows
    }

    /// Read one child's output through a character window. The parent log
    /// must hold the child's spawn boundary, so the read stays scoped to the
    /// caller's own fleet. The text is the child's assistant output joined
    /// over its durable log.
    pub(super) async fn fleet_output(
        &self,
        parent_sid: SessionId,
        child_id: &str,
        offset: usize,
        limit: usize,
    ) -> Result<ChildOutputView, String> {
        let sid = parse_session_id(child_id)?;
        let parent_log = self
            .store
            .replay(parent_sid)
            .await
            .map_err(|e| format!("the parent log could not be read: {e}"))?;
        let mut owned = false;
        let mut status = String::new();
        let mut summary: Option<String> = None;
        for entry in &parent_log {
            match &entry.event {
                SessionEvent::SubagentSpawn {
                    child_session_id, ..
                } if child_session_id == child_id => {
                    owned = true;
                }
                SessionEvent::SubagentReturn {
                    child_session_id,
                    status: s,
                    summary: m,
                    ..
                } if child_session_id == child_id => {
                    status = s.clone();
                    summary = if m.is_empty() { None } else { Some(m.clone()) };
                }
                _ => {}
            }
        }
        if !owned {
            return Err(format!("task_id {child_id} is not a child of this session"));
        }
        // A live runner means running; a dead Weak means the child finished
        // and its durable state stands.
        let running = self
            .children
            .lock()
            .map(|kids| {
                kids.get(child_id)
                    .and_then(|e| e.runner.upgrade())
                    .is_some()
            })
            .unwrap_or(false);
        if running {
            status = "running".into();
        }
        let child_log = self
            .store
            .replay(sid)
            .await
            .map_err(|e| format!("the child log could not be read: {e}"))?;
        let mut parts = Vec::new();
        for entry in &child_log {
            if let SessionEvent::AssistantMessage { text, .. } = &entry.event
                && !text.trim().is_empty()
            {
                parts.push(text.clone());
            }
        }
        let full = parts.join("\n\n");
        let total_chars = full.chars().count();
        let start = offset.min(total_chars);
        let end = start.saturating_add(limit).min(total_chars);
        let text: String = full.chars().skip(start).take(end - start).collect();
        Ok(ChildOutputView {
            child_session_id: child_id.to_string(),
            status,
            summary,
            text,
            total_chars,
            offset: start,
        })
    }
}

/// Parse a child id back into a session id, with the message the model reads.
fn parse_session_id(child_id: &str) -> Result<SessionId, String> {
    SessionId::from_display_string(child_id)
        .ok_or_else(|| format!("task_id {child_id} is not a session id"))
}
