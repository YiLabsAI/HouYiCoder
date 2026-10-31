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
use houyicoder_protocol::tool::is_user_shell_call_id;
#[cfg(debug_assertions)]
use std::collections::HashSet;

use crate::agent::context::retention::AssemblyScope;
use crate::agent::context::retention::RetentionContext;
use crate::agent::context::retention::materialize_block;
use crate::agent::context::retention::superseded_by_later;
use crate::agent::context::retention::{AgeRetentionPolicy, RetentionPolicy};
use crate::agent::context::shell_history::ShellHistoryPlan;
use crate::agent::context::shell_history::plan_shell_history;
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
/// In debug builds this guards the adjacency of a tool call and its result: a
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
/// already a User (providers reject user-in-a-row). Every caller is in this
/// module: the two assembly steps that append a user clause.
fn append_user_text(items: &mut Vec<InputItem>, text: &str) {
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
/// and its result. Appending it where the log holds it would split the call
/// from its answer;
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
        walk: RetentionWalk<'_>,
        plan: &ShellHistoryPlan,
    ) {
        for call_id in self.open.drain(..) {
            let output = match find_late_result(events, from, &call_id) {
                Some((idx, raw)) => materialize_result(events, idx, raw, walk, plan),
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
/// system-reminder variants served to the model in a user message. One list
/// decides which events form a message, so a window that cuts the conversation
/// and the assembly that serves it agree on where a message begins.
pub(crate) fn user_text_of(event: &SessionEvent) -> Option<String> {
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

/// The collaborators one assembly walk shares: the block store a result may
/// be read from, the policy that decides how it is served, and the assembly
/// the decisions belong to. Grouped so a helper that serves one result does
/// not carry three parameters of its own.
#[derive(Clone, Copy)]
struct RetentionWalk<'a> {
    backend: Option<&'a dyn ContextBackend>,
    policy: &'a dyn RetentionPolicy,
    scope: AssemblyScope,
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
    let mut seen_call_ids: HashSet<String> = HashSet::new();
    let plan = charged_shell_plan(events, policy, scope.id);
    let walk = RetentionWalk {
        backend,
        policy,
        scope,
    };
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
                held.finish_open_group(&mut items, events, i, walk, &plan);
                let (tool_calls, next) = gather_tool_calls(events, i + 1, &plan);
                #[cfg(debug_assertions)]
                for call in &tool_calls {
                    seen_call_ids.insert(call.id.clone());
                }
                for call in &tool_calls {
                    held.open_call(&call.id);
                }
                items.push(InputItem::Assistant {
                    content: text.clone(),
                    tool_calls,
                });
                i = next;
            }
            SessionEvent::ToolCall {
                call_id,
                tool,
                input,
            } => {
                if is_user_shell_call_id(call_id) && !plan.keeps(call_id) {
                    i += 1;
                    continue;
                }
                #[cfg(debug_assertions)]
                seen_call_ids.insert(call_id.clone());
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
                // The call was dropped with its group, so the result has no
                // call to answer and must not enter the request alone: a
                // result without its call is a request the provider rejects.
                if is_user_shell_call_id(call_id) && !plan.keeps(call_id) {
                    i += 1;
                    continue;
                }
                #[cfg(debug_assertions)]
                debug_assert!(
                    seen_call_ids.contains(call_id.as_str()),
                    "assemble_model_input: orphan ToolResult {call_id} has no matching ToolCall in the window — a compaction plan dropped the call and its answer"
                );
                if held.is_open(call_id) {
                    items.push(InputItem::ToolResult {
                        call_id: call_id.clone(),
                        output: materialize_result(events, i, output, walk, &plan),
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
    held.finish_open_group(&mut items, events, events.len(), walk, &plan);
    append_shell_notice(&mut items, &plan);
    items
}

/// The tool calls an assistant message carries: the consecutive call events
/// after it that the request keeps. A folded command is skipped, and the walk
/// runs past it so the calls after it still join this message. The returned
/// index is where the walk stopped: the first event that is not a call.
fn gather_tool_calls(
    events: &[SessionLogEntry],
    start: usize,
    plan: &ShellHistoryPlan,
) -> (Vec<AssistantToolCall>, usize) {
    let mut calls = Vec::new();
    let mut index = start;
    while index < events.len() {
        let SessionEvent::ToolCall {
            call_id,
            tool,
            input,
        } = &events[index].event
        else {
            break;
        };
        if !is_user_shell_call_id(call_id) || plan.keeps(call_id) {
            calls.push(AssistantToolCall {
                id: call_id.clone(),
                name: tool.clone(),
                input: input.clone(),
            });
        }
        index += 1;
    }
    (calls, index)
}

/// Decide the command history before the walk, so the tokens it commits are
/// charged before any block retrieval is decided: text the request already
/// carries must not also be spent on a retrieval.
///
/// The plan pays for each kept command as the log holds it, never for the
/// stored length of a block it names, and the retention pre-check admits that
/// block against what the plan left unspent. One result therefore reaches the
/// request once: either as the logged text the plan accounted, or as a
/// retrieval charged at the moment it is decided.
fn charged_shell_plan(
    events: &[SessionLogEntry],
    policy: &dyn RetentionPolicy,
    assembly_id: u64,
) -> ShellHistoryPlan {
    let budget = policy.shell_history_budget();
    let plan = plan_shell_history(events, budget, &|text| policy.count_tokens(text));
    policy.charge_assembly_tokens(assembly_id, plan.charged_tokens);
    tracing::debug!(
        assembly = assembly_id,
        charged = plan.charged_tokens,
        budget,
        kept = plan.kept_count(),
        folded = plan.notice.is_some(),
        "user shell history planned"
    );
    plan
}

/// The one line describing the folded commands rides at the tail: every kept
/// call and its answer then render exactly as before, so the notice costs no cache
/// prefix, and a turn whose folding changed still reuses the history above it.
fn append_shell_notice(items: &mut Vec<InputItem>, plan: &ShellHistoryPlan) {
    if let Some(notice) = &plan.notice {
        append_user_text(items, notice);
    }
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
    walk: RetentionWalk<'_>,
    plan: &ShellHistoryPlan,
) -> serde_json::Value {
    let age_in_turns = events[i + 1..]
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::AssistantMessage { .. }))
        .count() as u32;
    let is_superseded = superseded_by_later(events, i);
    // The plan may already count this result as the log holds it. A retrieval
    // of the block replaces that logged form, so the decision needs the term
    // to keep one result from spending the assembly's quota twice.
    let logged_result_tokens = match &events[i].event {
        SessionEvent::ToolResult { call_id, .. } => plan.charged_result_tokens(call_id),
        _ => None,
    };
    let ctx = RetentionContext {
        age_in_turns,
        is_superseded,
        block_ref: None,
        stored_bytes: None,
        logged_result_tokens,
        assembly_id: walk.scope.id,
        mutating: walk.scope.mutating,
    };
    materialize_block(output, walk.backend, walk.policy, &ctx)
}

#[cfg(test)]
mod tests;
