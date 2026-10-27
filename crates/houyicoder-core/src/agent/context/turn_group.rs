//! Assemble model input from the durable session log.
//!
//! The agent rebuilds messages for each request rather than keeping a second
//! mutable conversation buffer.
//!
//! Assistant text and its tool calls form one message. A tool result must
//! refer to a call retained in the same context window.
//!
//! Background events may arrive between a call and its result. User-role text
//! is held until the known results arrive; this changes only the request, not
//! event order on disk. Incomplete call groups still require handling before
//! sending a request to a strict provider.

use houyicoder_context::{ContextBackend, SessionEvent, SessionLogEntry};
use houyicoder_protocol::llm::{AssistantToolCall, InputItem};
#[cfg(debug_assertions)]
use std::collections::HashSet;

use crate::agent::context::retention::AssemblyScope;
use crate::agent::context::retention::RetentionContext;
use crate::agent::context::retention::materialize_block;
use crate::agent::context::retention::superseded_by_later;
use crate::agent::context::retention::{AgeRetentionPolicy, RetentionPolicy};
use crate::agent::skill_body::frame_untrusted_body;

/// How an assembly relates to the provider request it serves. A turn that
/// will be sent advances the assembly id and may record decisions; a
/// measurement reads without recording; a length-recovery continuation reuses
/// the id so it does not consume a recompute mid-sentence.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AssemblyMode {
    pub(crate) mutating: bool,
    pub(crate) continuation: bool,
}

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
    assemble_model_input_with(
        events,
        backend,
        &AgeRetentionPolicy::default(),
        AssemblyScope {
            id: 0,
            mutating: false,
        },
    )
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

    /// True when the call is still awaiting its tool result in the current
    /// assistant group. A result for a call that was already closed (or never
    /// opened in this window) must not be emitted a second time.
    fn is_open(&self, call_id: &str) -> bool {
        self.open.iter().any(|id| id == call_id)
    }

    /// Answer a call, releasing the held text once none is left open.
    fn close_call(&mut self, items: &mut Vec<InputItem>, call_id: &str) {
        self.open.retain(|id| id != call_id);
        if self.open.is_empty() {
            self.flush_held(items);
        }
    }

    /// Close any still-open calls before a new assistant message or at the end
    /// of the window. If a matching tool result appears later in the log, pull
    /// that real result forward; otherwise emit a stub result so the assistant
    /// tool-call message is never left unanswered.
    fn finish_open_group(
        &mut self,
        items: &mut Vec<InputItem>,
        events: &[SessionLogEntry],
        from: usize,
        backend: Option<&dyn ContextBackend>,
        policy: &dyn RetentionPolicy,
        scope: AssemblyScope,
    ) {
        for call_id in self.open.drain(..) {
            let output = match find_late_result(events, from, &call_id) {
                Some((idx, raw)) => materialize_result(events, idx, raw, backend, policy, scope),
                None => serde_json::json!({"error": "no tool result recorded"}),
            };
            items.push(InputItem::ToolResult { call_id, output });
        }
        self.flush_held(items);
    }

    /// Append the held user text in arrival order.
    fn flush_held(&mut self, items: &mut Vec<InputItem>) {
        for text in self.held.drain(..) {
            append_user_text(items, &text);
        }
    }
}

/// Find a tool result for call_id at or after index from in the event slice.
fn find_late_result<'a>(
    events: &'a [SessionLogEntry],
    from: usize,
    target_id: &str,
) -> Option<(usize, &'a serde_json::Value)> {
    events[from..]
        .iter()
        .enumerate()
        .find_map(|(offset, entry)| match &entry.event {
            SessionEvent::ToolResult {
                call_id, output, ..
            } if call_id == target_id => Some((from + offset, output)),
            _ => None,
        })
}

/// Extract user-role text from an event when it is one of the user or
/// system-reminder variants served to the model in a user message.
fn user_text_of(event: &SessionEvent) -> Option<String> {
    match event {
        // Merge consecutive user messages: providers reject multiple
        // user-in-a-row. MetaUser (runner nudge) + MemoryRecall
        // (system-reminder memories) + SkillListing (system-reminder
        // skill catalog) are served to the model identically; the
        // transcript-skip happens in the host records assembly.
        SessionEvent::UserInput { text }
        | SessionEvent::MetaUser { text }
        | SessionEvent::MemoryRecall { text, .. }
        | SessionEvent::SkillListing { text, .. } => Some(text.clone()),
        SessionEvent::SkillBody {
            skill_name,
            content,
            untrusted,
            ..
        } => Some(frame_untrusted_body(skill_name, content, *untrusted)),
        SessionEvent::MidTurnInput { text, .. } => Some(format!(
            "[The user sent this message while you were working. \
             Continue your current task and address it when natural.]\n\
             {text}"
        )),
        SessionEvent::NotificationInjected { summary, .. } => Some(format!(
            "[A background subagent completed. Act on its result if \
             relevant to the current task.]\n{summary}"
        )),
        _ => None,
    }
}

/// Join a bare tool call onto the trailing assistant message when one is open,
/// or start a new assistant message when the window began mid-group.
fn push_tool_call(items: &mut Vec<InputItem>, call: AssistantToolCall) {
    match items.last_mut() {
        Some(InputItem::Assistant { tool_calls, .. }) => tool_calls.push(call),
        _ => items.push(InputItem::Assistant {
            content: String::new(),
            tool_calls: vec![call],
        }),
    }
}

/// Assemble the model-input history from a replayed event log with an explicit
/// retention policy and assembly id. The cache-liveness policy scopes a batch
/// recompute to the assembly boundary; the default path (tests and the
/// no-cache view) passes the age policy and a fixed id, which never reports a
/// live cache.
pub fn assemble_model_input_with(
    events: &[SessionLogEntry],
    backend: Option<&dyn ContextBackend>,
    policy: &dyn RetentionPolicy,
    scope: AssemblyScope,
) -> Vec<InputItem> {
    #[cfg(debug_assertions)]
    let mut seen_call_ids: HashSet<&str> = HashSet::new();
    let mut items = Vec::with_capacity(events.len());
    let mut i = 0;
    let mut held = HeldUserText::default();
    while i < events.len() {
        if let Some(text) = user_text_of(&events[i].event) {
            held.push(&mut items, &text);
            i += 1;
            continue;
        }
        match &events[i].event {
            SessionEvent::AssistantMessage { text, .. } => {
                held.finish_open_group(&mut items, events, i, backend, policy, scope);
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
                push_tool_call(
                    &mut items,
                    AssistantToolCall {
                        id: call_id.clone(),
                        name: tool.clone(),
                        input: input.clone(),
                    },
                );
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
                if held.is_open(call_id) {
                    items.push(InputItem::ToolResult {
                        call_id: call_id.clone(),
                        output: materialize_result(events, i, output, backend, policy, scope),
                    });
                    held.close_call(&mut items, call_id);
                }
                i += 1;
            }
            // Display and audit only: a recap never enters the model input,
            // so the model cannot read its own earlier summary as a fresh
            // instruction or as evidence that work finished.
            SessionEvent::RecapAttempt { .. } | SessionEvent::AwayRecap { .. } => {
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    held.finish_open_group(&mut items, events, events.len(), backend, policy, scope);
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
    scope: AssemblyScope,
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
        assembly_id: scope.id,
        mutating: scope.mutating,
    };
    materialize_block(output, backend, policy, &ctx)
}

#[cfg(test)]
mod tests;
