//! Turn grouping: project a replayed event log into the model-input history.
//!
//! This is the "transcript-as-truth" seam: the loop holds no in-memory
//! message buffer — it re-reads the event log each turn and projects.
//!
//! Grouping: one AssistantMessage event plus its immediately-following
//! ToolCall events collapse into a single Assistant InputItem (content =
//! the text, tool_calls = the calls). This reconstructs one API assistant
//! message with its tool_use blocks. A subsequent ToolResult event becomes
//! a ToolResult InputItem referencing the call_id — the
//! tool_use/tool_result pair invariant must hold in the window (a
//! ToolResult whose ToolCall was compacted out is a view bug, not a
//! projection bug).
//!
//! The integral-group boundary: thinking + tool_use share a Disposition
//! (an API constraint — they share the same fate; an assistant message
//! and its thinking must travel together). Orphaned thinking-only messages
//! introduced by compaction slicing are post-cleared. Type-First encoding
//! keeps thinking inside the integral group so it is never orphaned by a
//! later slice. tool_result can be an independent CAS pointer (the
//! Isolate stage, not Compress), so a block_ref marker does not break the
//! integral group.

use houyicoder_context::{ContextBackend, TurnEvent, TurnEventKind};
use houyicoder_protocol::llm::{AssistantToolCall, InputItem};
#[cfg(debug_assertions)]
use std::collections::HashSet;

use super::retention::{AgeRetentionPolicy, RetentionPolicy};

/// Project a replayed event log into the model-input history, in order.
///
/// In debug builds this guards the tool_use/tool_result pair invariant: a
/// ToolResult whose ToolCall is not in the same window trips a
/// debug_assert!. full-replay never triggers it; it fires the first time
/// a compaction plan drops a ToolCall while keeping its ToolResult — the
/// kind of silent break that would otherwise surface as a provider "tool_result
/// without tool_use" error deep in a run.
pub fn project_input_items(
    events: &[TurnEvent],
    backend: Option<&dyn ContextBackend>,
) -> Vec<InputItem> {
    project_input_items_with(events, backend, &AgeRetentionPolicy::default(), 0)
}

/// Push a user-role text into the items, merging into the last item if it is
/// already a User (providers reject user-in-a-row).
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

/// Project a replayed event log into the model-input history with an explicit
/// retention policy + wall-clock now. The cache-liveness policy uses now to
/// test the cached-prefix TTL; the default path (tests + the no-cache view)
/// passes the age policy + 0, which never reports a live cache.
#[expect(
    clippy::too_many_lines,
    reason = "exhaustive match on a growing event enum; new variant arms pushed past the limit"
)]
pub fn project_input_items_with(
    events: &[TurnEvent],
    backend: Option<&dyn ContextBackend>,
    policy: &dyn RetentionPolicy,
    now_ms: u64,
) -> Vec<InputItem> {
    #[cfg(debug_assertions)]
    let mut seen_call_ids: HashSet<&str> = HashSet::new();
    let mut items = Vec::with_capacity(events.len());
    let mut i = 0;
    while i < events.len() {
        match &events[i].kind {
            // Merge consecutive user messages: providers reject multiple
            // user-in-a-row. MetaUser (runner nudge) + MemoryRecall
            // (system-reminder memories) + SkillListing (system-reminder
            // skill catalog) are served to the model identically; the
            // transcript-skip happens in the host records projection.
            TurnEventKind::UserInput { text }
            | TurnEventKind::MetaUser { text }
            | TurnEventKind::MemoryRecall { text, .. }
            | TurnEventKind::SkillListing { text, .. } => {
                append_user_text(&mut items, text);
                i += 1;
            }
            TurnEventKind::SkillBody { content, .. } => {
                append_user_text(&mut items, content);
                i += 1;
            }
            // Mid-work interjection: wrap with a framing note so the model
            // reads "continue the task + address", not a fresh instruction
            // that drops the in-flight task. The bare text stays in the
            // durable log + transcript; the framing is model-only.
            TurnEventKind::MidTurnInput { text } => {
                let framed = format!(
                    "[The user sent this message while you were working. \
                     Continue your current task and address it when natural.]\n\
                     {text}"
                );
                append_user_text(&mut items, &framed);
                i += 1;
            }
            // Reward observations are audit signals, not model input.
            TurnEventKind::RewardObservation { .. } => i += 1,
            TurnEventKind::Unknown => i += 1,
            TurnEventKind::AssistantMessage { text, .. } => {
                let mut tool_calls = Vec::new();
                let mut j = i + 1;
                while j < events.len() {
                    match &events[j].kind {
                        TurnEventKind::ToolCall {
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
                items.push(InputItem::Assistant {
                    content: text.clone(),
                    tool_calls,
                });
                i = j;
            }
            TurnEventKind::ToolCall {
                call_id,
                tool,
                input,
            } => {
                #[cfg(debug_assertions)]
                seen_call_ids.insert(call_id.as_str());
                items.push(InputItem::Assistant {
                    content: String::new(),
                    tool_calls: vec![AssistantToolCall {
                        id: call_id.clone(),
                        name: tool.clone(),
                        input: input.clone(),
                    }],
                });
                i += 1;
            }
            TurnEventKind::ToolResult {
                call_id, output, ..
            } => {
                #[cfg(debug_assertions)]
                debug_assert!(
                    seen_call_ids.contains(call_id.as_str()),
                    "project_input_items: orphan ToolResult {call_id} has no matching ToolCall in the window — a compaction plan dropped the pair"
                );
                items.push(InputItem::ToolResult {
                    call_id: call_id.clone(),
                    output: materialize_result(events, i, output, backend, policy, now_ms),
                });
                i += 1;
            }
            TurnEventKind::Reasoning { .. }
            | TurnEventKind::AssistantTextDelta { .. }
            | TurnEventKind::CompactionBoundary { .. }
            | TurnEventKind::CacheBreak { .. }
            | TurnEventKind::Summary { .. }
            | TurnEventKind::PermissionDecision { .. }
            | TurnEventKind::TurnAborted { .. }
            | TurnEventKind::TruncationVerdict { .. }
            | TurnEventKind::WorktreeEnter { .. }
            | TurnEventKind::WorktreeExit { .. }
            | TurnEventKind::TurnUsage { .. }
            | TurnEventKind::HookSignal { .. }
            | TurnEventKind::TurnStarted { .. }
            | TurnEventKind::SubagentSpawn { .. }
            | TurnEventKind::SubagentReturn { .. }
            | TurnEventKind::NotificationInjected { .. } => {
                i += 1;
            }
        }
    }
    items
}

/// Materialize a ToolResult output under the retention policy. The age in
/// turns (how many assistant rounds follow this result in the window) drives
/// the 3-tier decision: recent results materialize in full, middle-aged
/// results summarize to the inline preview, old or superseded results evict
/// to the pointer. Supersession = a later tool call for the same resource
/// (a re-read of the same file, a re-run of the same command).
fn materialize_result(
    events: &[TurnEvent],
    i: usize,
    output: &serde_json::Value,
    backend: Option<&dyn ContextBackend>,
    policy: &dyn RetentionPolicy,
    now_ms: u64,
) -> serde_json::Value {
    let age_in_turns = events[i + 1..]
        .iter()
        .filter(|e| matches!(e.kind, TurnEventKind::AssistantMessage { .. }))
        .count() as u32;
    let is_superseded = super::retention::superseded_by_later(events, i);
    let ctx = super::retention::RetentionContext {
        age_in_turns,
        is_superseded,
        block_ref: None,
        now_ms,
        cache_cold: false,
    };
    super::retention::materialize_block(output, backend, policy, &ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::{EventId, SessionId};

    fn evt(kind: TurnEventKind) -> TurnEvent {
        TurnEvent {
            id: EventId::new(),
            session: SessionId::new(),
            ts: 0,
            prev_hash: None,
            kind,
        }
    }

    fn user(text: &str) -> TurnEventKind {
        TurnEventKind::UserInput { text: text.into() }
    }

    #[test]
    fn test_projects_user_assistant_toolresult() {
        let events = vec![
            evt(TurnEventKind::UserInput { text: "hi".into() }),
            evt(TurnEventKind::AssistantMessage {
                text: "let me echo".into(),
                thinking: None,
            }),
            evt(TurnEventKind::ToolCall {
                call_id: "c1".into(),
                tool: "echo".into(),
                input: serde_json::json!({"x": 1}),
            }),
            evt(TurnEventKind::tool_result(
                "c1",
                serde_json::json!({"echo": {"x": 1}}),
            )),
        ];
        let items = project_input_items(&events, None);
        assert_eq!(items.len(), 3);
        assert!(matches!(items[0], InputItem::User { .. }));
        match &items[1] {
            InputItem::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content, "let me echo");
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].name, "echo");
            }
            _ => panic!("expected assistant item"),
        }
        assert!(matches!(items[2], InputItem::ToolResult { .. }));
    }

    #[test]
    fn test_assistant_no_tools_empty() {
        let events = vec![
            evt(TurnEventKind::UserInput { text: "hi".into() }),
            evt(TurnEventKind::AssistantMessage {
                text: "hello".into(),
                thinking: None,
            }),
        ];
        let items = project_input_items(&events, None);
        match &items[1] {
            InputItem::Assistant { tool_calls, .. } => assert!(tool_calls.is_empty()),
            _ => panic!(),
        }
    }

    #[test]
    fn test_skips_reasoning_and_compaction() {
        let events = vec![
            evt(TurnEventKind::Reasoning {
                text: "thinking".into(),
            }),
            evt(TurnEventKind::UserInput { text: "hi".into() }),
            evt(TurnEventKind::Summary { text: "old".into() }),
        ];
        let items = project_input_items(&events, None);
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], InputItem::User { .. }));
    }

    #[test]
    fn test_skips_assistant_text_deltas() {
        // Streaming deltas are subsumed by the authoritative AssistantMessage
        // that follows them. Projection must skip every delta so the model-input
        // history carries one assistant message, not N fragments plus a dupe.
        let events = vec![
            evt(TurnEventKind::UserInput { text: "hi".into() }),
            evt(TurnEventKind::AssistantTextDelta { text: "hel".into() }),
            evt(TurnEventKind::AssistantTextDelta { text: "lo".into() }),
            evt(TurnEventKind::AssistantMessage {
                text: "hello".into(),
                thinking: None,
            }),
        ];
        let items = project_input_items(&events, None);
        assert_eq!(items.len(), 2);
        match &items[1] {
            InputItem::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content, "hello");
                assert!(tool_calls.is_empty());
            }
            _ => panic!("expected one assistant item, got {:?}", items[1]),
        }
    }

    #[test]
    fn test_recall_merges_into_user() {
        // A MemoryRecall event following a UserInput merges into that User
        // item so the model sees the query plus the recalled-memory
        // attachment as one user message — the cross-layer chain's
        // load-bearing merge. If this regressed, /context accounting could
        // still pass (it counts events, not projected messages) while the
        // model never sees the recalled memory.
        let s = SessionId::new();
        let ids = (0..2).map(|_| EventId::new()).collect::<Vec<_>>();
        let events = vec![
            TurnEvent {
                id: ids[0],
                session: s,
                ts: 0,
                prev_hash: None,
                kind: user("what is the deploy command"),
            },
            TurnEvent {
                id: ids[1],
                session: s,
                ts: 0,
                prev_hash: None,
                kind: TurnEventKind::MemoryRecall {
                    text: "<system-reminder>deploy: make deploy</system-reminder>".into(),
                    keys: vec!["deploy".into()],
                    bytes: 0,
                },
            },
        ];
        let items = project_input_items(&events, None);
        assert_eq!(items.len(), 1, "recall merges into the user item");
        match &items[0] {
            InputItem::User { content } => {
                assert!(content.contains("deploy command"), "query preserved");
                assert!(
                    content.contains("make deploy"),
                    "recalled memory appended to the user item"
                );
            }
            _ => panic!("expected merged user item, got {:?}", items[0]),
        }
    }

    /// An Unknown kind (a future-binary event type) is skipped by the
    /// grouping loop — it produces no input item and does not break the
    /// surrounding turn's grouping.
    #[test]
    fn test_unknown_kind_skipped() {
        let events = vec![
            evt(TurnEventKind::UserInput { text: "hi".into() }),
            evt(TurnEventKind::Unknown),
            evt(TurnEventKind::AssistantMessage {
                text: "reply".into(),
                thinking: None,
            }),
        ];
        let items = project_input_items(&events, None);
        assert_eq!(items.len(), 2, "Unknown skipped, no input item");
        assert!(matches!(items[0], InputItem::User { .. }));
        assert!(matches!(items[1], InputItem::Assistant { .. }));
    }
}
