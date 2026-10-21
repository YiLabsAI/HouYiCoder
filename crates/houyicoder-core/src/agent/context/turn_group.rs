//! Turn grouping: assemble the model-input history from a replayed event log.
//!
//! This is the "transcript-as-truth" seam: the loop holds no in-memory
//! message buffer — it re-reads the event log each turn and assembles input.
//!
//! Grouping: one AssistantMessage event plus its immediately-following
//! ToolCall events collapse into a single Assistant InputItem (content =
//! the text, tool_calls = the calls). This reconstructs one API assistant
//! message with its tool_use blocks. A subsequent ToolResult event becomes
//! a ToolResult InputItem referencing the call_id — the
//! tool_use/tool_result pair invariant must hold in the window (a
//! ToolResult whose ToolCall was compacted out is a view bug, not an
//! assembly bug).
//!
//! Adjacency is a property of the projection, not of the log. Background
//! work appends whenever it finishes, so user-role text can land between a
//! call and its result; the request is then invalid, because the assistant
//! message carrying the calls is no longer followed by one tool message per
//! call. Such text is held until every open call is answered, which keeps
//! both the log and the request whole.
//!
//! The integral-group boundary: thinking + tool_use share a Disposition
//! (an API constraint — they share the same fate; an assistant message
//! and its thinking must travel together). Orphaned thinking-only messages
//! introduced by compaction slicing are post-cleared. Type-First encoding
//! keeps thinking inside the integral group so it is never orphaned by a
//! later slice. tool_result can be an independent CAS pointer (the
//! Isolate stage, not Compress), so a block_ref marker does not break the
//! integral group.

use houyicoder_context::{ContextBackend, SessionEvent, SessionLogEntry};
use houyicoder_protocol::llm::{AssistantToolCall, InputItem};
#[cfg(debug_assertions)]
use std::collections::HashSet;

use crate::agent::retention::RetentionContext;
use crate::agent::retention::materialize_block;
use crate::agent::retention::superseded_by_later;
use crate::agent::retention::{AgeRetentionPolicy, RetentionPolicy};
use crate::agent::skill_body::frame_untrusted_body;

/// Assemble the model-input history from a replayed event log, in order.
///
/// In debug builds this guards the tool_use/tool_result pair invariant: a
/// ToolResult whose ToolCall is not in the same window trips a
/// debug_assert!. full-replay never triggers it; it fires the first time
/// a compaction plan drops a ToolCall while keeping its ToolResult — the
/// kind of silent break that would otherwise surface as a provider "tool_result
/// without tool_use" error deep in a run.
pub fn assemble_model_input(
    events: &[SessionLogEntry],
    backend: Option<&dyn ContextBackend>,
) -> Vec<InputItem> {
    assemble_model_input_with(events, backend, &AgeRetentionPolicy::default(), 0)
}

/// Push a user-role text into the items, merging into the last item if it is
/// already a User (providers reject user-in-a-row). Shared with the loop that
/// appends a runner nudge after assembly, so both keep the same rule.
pub(crate) fn append_user_text(items: &mut Vec<InputItem>, text: &str) {
    if let Some(InputItem::User { content }) = items.last_mut() {
        content.push('\n');
        content.push_str(text);
    } else {
        items.push(InputItem::User {
            content: text.to_string(),
        });
    }
}

/// User-role text that arrived while tool results were still outstanding.
///
/// A request is rejected when an assistant message carrying tool calls is not
/// answered by one tool message per call straight after it. Background work
/// appends at any point, so a recall or an interjection can land between a call
/// and its result. Appending it where the log holds it would split the pair;
/// dropping it would lose a message the user or the runtime wrote. The queue
/// keeps such text until every open call is answered, so the projection stays
/// valid while the durable log keeps its own order.
#[derive(Default)]
struct HeldUserText {
    /// Call ids whose result has not been appended yet.
    open: Vec<String>,
    /// Text waiting for those calls to close.
    held: Vec<String>,
}

impl HeldUserText {
    /// Append text now, or hold it while a call is open.
    fn push(&mut self, items: &mut Vec<InputItem>, text: &str) {
        if self.open.is_empty() {
            append_user_text(items, text);
        } else {
            self.held.push(text.to_string());
        }
    }

    /// Record a call the request now has to answer.
    fn open_call(&mut self, call_id: &str) {
        if !self.open.iter().any(|id| id == call_id) {
            self.open.push(call_id.to_string());
        }
    }

    /// Answer a call, releasing the held text once none is left open.
    fn close_call(&mut self, items: &mut Vec<InputItem>, call_id: &str) {
        self.open.retain(|id| id != call_id);
        if self.open.is_empty() {
            self.release(items);
        }
    }

    /// Start a new assistant message. The calls of the previous one are not
    /// answered before this reply, so the text they held is released ahead of
    /// it, where the user wrote it.
    fn next_message(&mut self, items: &mut Vec<InputItem>) {
        self.open.clear();
        self.release(items);
    }

    /// Append the held text in arrival order.
    fn release(&mut self, items: &mut Vec<InputItem>) {
        for text in self.held.drain(..) {
            append_user_text(items, &text);
        }
    }
}

/// Assemble the model-input history from a replayed event log with an explicit
/// retention policy + wall-clock now. The cache-liveness policy uses now to
/// test the cached-prefix TTL; the default path (tests + the no-cache view)
/// passes the age policy + 0, which never reports a live cache.
#[expect(
    clippy::too_many_lines,
    reason = "exhaustive match on a growing event enum; new variant arms pushed past the limit"
)]
pub fn assemble_model_input_with(
    events: &[SessionLogEntry],
    backend: Option<&dyn ContextBackend>,
    policy: &dyn RetentionPolicy,
    now_ms: u64,
) -> Vec<InputItem> {
    #[cfg(debug_assertions)]
    let mut seen_call_ids: HashSet<&str> = HashSet::new();
    let mut items = Vec::with_capacity(events.len());
    let mut i = 0;
    let mut held = HeldUserText::default();
    while i < events.len() {
        match &events[i].event {
            // Merge consecutive user messages: providers reject multiple
            // user-in-a-row. MetaUser (runner nudge) + MemoryRecall
            // (system-reminder memories) + SkillListing (system-reminder
            // skill catalog) are served to the model identically; the
            // transcript-skip happens in the host records assembly.
            SessionEvent::UserInput { text }
            | SessionEvent::MetaUser { text }
            | SessionEvent::MemoryRecall { text, .. }
            | SessionEvent::SkillListing { text, .. } => {
                held.push(&mut items, text);
                i += 1;
            }
            SessionEvent::SkillBody {
                skill_name,
                content,
                untrusted,
                ..
            } => {
                // An untrusted body (a non-managed/user source) is framed
                // as data, not trusted instruction, so the model treats
                // its directives as unverified and confirms before acting
                // on state-changing steps. The framing helper is shared
                // with the Skill tool path so framing does not differ by
                // invocation path.
                let text = frame_untrusted_body(skill_name, content, *untrusted);
                held.push(&mut items, &text);
                i += 1;
            }
            // Mid-work interjection: wrap with a framing note so the model
            // reads "continue the task + address", not a fresh instruction
            // that drops the in-flight task. The bare text stays in the
            // durable log + transcript; the framing is model-only.
            SessionEvent::MidTurnInput { text, .. } => {
                let framed = format!(
                    "[The user sent this message while you were working. \
                     Continue your current task and address it when natural.]\n\
                     {text}"
                );
                held.push(&mut items, &framed);
                i += 1;
            }
            // A background subagent finished: the notification carries the
            // child's result summary. Frame it as a child-completion report
            // (not a user interjection) so the model reads it as a background
            // result to act on, not a fresh user instruction that drops the
            // in-flight task. The bare summary stays in the durable log; the
            // framing is model-only.
            SessionEvent::NotificationInjected { summary, .. } => {
                let framed = format!(
                    "[A background subagent completed. Act on its result if \
                     relevant to the current task.]\n{summary}"
                );
                held.push(&mut items, &framed);
                i += 1;
            }
            // Reward observations are audit signals, not model input.
            SessionEvent::RewardObservation { .. } => i += 1,
            SessionEvent::Unknown => i += 1,
            SessionEvent::AssistantMessage { text, .. } => {
                held.next_message(&mut items);
                let mut tool_calls = Vec::new();
                let mut j = i + 1;
                while j < events.len() {
                    match &events[j].event {
                        SessionEvent::ToolCall {
                            call_id,
                            tool,
                            input,
                        } => {
                            #[cfg(debug_assertions)]
                            seen_call_ids.insert(call_id.as_str());
                            tool_calls.push(AssistantToolCall {
                                id: call_id.clone(),
                                name: tool.clone(),
                                input: input.clone(),
                            });
                            j += 1;
                        }
                        _ => break,
                    }
                }
                for call in &tool_calls {
                    held.open_call(&call.id);
                }
                items.push(InputItem::Assistant {
                    content: text.clone(),
                    tool_calls,
                });
                i = j;
            }
            SessionEvent::ToolCall {
                call_id,
                tool,
                input,
            } => {
                #[cfg(debug_assertions)]
                seen_call_ids.insert(call_id.as_str());
                held.open_call(call_id);
                let call = AssistantToolCall {
                    id: call_id.clone(),
                    name: tool.clone(),
                    input: input.clone(),
                };
                // The calls of one assistant message are appended one at a
                // time, so background work can land between two of them and
                // stop the scan above. Joining the trailing assistant item
                // keeps them one message, which is what the provider needs;
                // a window that starts mid-group has no item to join, so it
                // opens one.
                match items.last_mut() {
                    Some(InputItem::Assistant { tool_calls, .. }) => tool_calls.push(call),
                    _ => items.push(InputItem::Assistant {
                        content: String::new(),
                        tool_calls: vec![call],
                    }),
                }
                i += 1;
            }
            SessionEvent::ToolResult {
                call_id, output, ..
            } => {
                #[cfg(debug_assertions)]
                debug_assert!(
                    seen_call_ids.contains(call_id.as_str()),
                    "assemble_model_input: orphan ToolResult {call_id} has no matching ToolCall in the window — a compaction plan dropped the pair"
                );
                items.push(InputItem::ToolResult {
                    call_id: call_id.clone(),
                    output: materialize_result(events, i, output, backend, policy, now_ms),
                });
                held.close_call(&mut items, call_id);
                i += 1;
            }
            SessionEvent::Reasoning { .. }
            | SessionEvent::AssistantTextDelta { .. }
            | SessionEvent::CompactionBoundary { .. }
            | SessionEvent::CacheBreak { .. }
            | SessionEvent::Summary { .. }
            | SessionEvent::PermissionDecision { .. }
            | SessionEvent::TurnAborted { .. }
            | SessionEvent::TruncationVerdict { .. }
            | SessionEvent::WorktreeEnter { .. }
            | SessionEvent::WorktreeExit { .. }
            | SessionEvent::TurnUsage { .. }
            | SessionEvent::HookSignal { .. }
            | SessionEvent::TurnStarted { .. }
            | SessionEvent::SubagentSpawn { .. }
            | SessionEvent::SubagentReturn { .. }
            | SessionEvent::ChildDelegated { .. }
            | SessionEvent::RunCompleted { .. }
            | SessionEvent::ModelStepTiming { .. }
            | SessionEvent::ContextCleared { .. } => {
                i += 1;
            }
        }
    }
    held.release(&mut items);
    items
}

/// Materialize a ToolResult output under the retention policy. The age in
/// turns (how many assistant rounds follow this result in the window) drives
/// the 3-tier decision: recent results materialize in full, middle-aged
/// results summarize to the inline preview, old or superseded results evict
/// to the pointer. Supersession = a later tool call for the same resource
/// (a re-read of the same file, a re-run of the same command).
fn materialize_result(
    events: &[SessionLogEntry],
    i: usize,
    output: &serde_json::Value,
    backend: Option<&dyn ContextBackend>,
    policy: &dyn RetentionPolicy,
    now_ms: u64,
) -> serde_json::Value {
    let age_in_turns = events[i + 1..]
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::AssistantMessage { .. }))
        .count() as u32;
    let is_superseded = superseded_by_later(events, i);
    let ctx = RetentionContext {
        age_in_turns,
        is_superseded,
        block_ref: None,
        now_ms,
    };
    materialize_block(output, backend, policy, &ctx)
}

#[cfg(test)]
mod tests;
