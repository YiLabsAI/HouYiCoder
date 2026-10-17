//! Tests for this module.

use super::*;
use crate::acpx::AcpxMethod;
use crate::frontend::memory::{
    MemoryChange, MemoryChangeCausality, MemoryChangeId, MemoryChangeOrigin, MemoryChangeScope,
    MemoryOperation,
};
use crate::frontend::queue::QueuedInput;
use crate::frontend::session_update::{
    AvailableCommand, AvailableCommandInput, ConfigOptionUpdate, CurrentModeUpdate, PlanEntry,
    PlanEntryPriority, PlanEntryStatus, SessionInfoUpdate, ToolCallId, ToolCallLocation,
    ToolCallUpdateFields, UsageUpdate,
};
use serde_json::{Value, json};

/// The metadata a producer attaches, as the field holds it.
fn meta(entries: &[(&str, &str)]) -> Meta {
    entries
        .iter()
        .map(|(key, text)| ((*key).to_string(), Value::String(text.to_string())))
        .collect()
}

/// The length of the filler every payload in the corpus test carries, so one
/// floor covers a payload held as text, as JSON, or as both at once.
const FILLER_BYTES: usize = 64;

/// One update per variant, each holding the same filler. A variant absent from
/// the walk reports nothing no matter what it holds, so a holder that retains a
/// window of that variant never reaches its byte budget. The list also drives
/// every private walk: a chunk, a tool call with its arguments and output, a
/// tool result, a plan with meta, the commands a session advertises, and the
/// options it reports.
#[test]
fn test_update_carries_payload() {
    let filler = "p".repeat(FILLER_BYTES);
    let text = || ContentBlock::Text {
        text: filler.clone(),
    };
    let mut call = ToolCall::new("call-1", filler.clone());
    call.content
        .push(ToolCallContent::Content { content: text() });
    call.locations.push(ToolCallLocation::new(filler.clone()));
    call.raw_input = Some(json!({ "count": 1, "force": true, "note": null }));
    call.raw_output = Some(json!(["first", filler.clone()]));

    let updates = vec![
        SessionUpdate::UserMessageChunk(ContentChunk::new(text())),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(text())),
        SessionUpdate::AgentThoughtChunk(ContentChunk::new(text())),
        SessionUpdate::ToolCall(call),
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId("call-1".to_string()),
            ToolCallUpdateFields {
                status: None,
                title: Some(filler.clone()),
                raw_input: Some(json!({ "path": filler.clone() })),
                raw_output: Some(json!({ "stdout": filler.clone() })),
            },
        )),
        SessionUpdate::Plan(Plan {
            entries: vec![PlanEntry {
                content: filler.clone(),
                priority: PlanEntryPriority::High,
                status: PlanEntryStatus::Pending,
                meta: Some(meta(&[("note", &filler)])),
            }],
            meta: Some(meta(&[("source", &filler)])),
        }),
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate {
            available_commands: vec![AvailableCommand {
                name: filler.clone(),
                description: filler.clone(),
                input: Some(AvailableCommandInput {
                    hint: filler.clone(),
                    meta: Some(meta(&[("arg", &filler)])),
                }),
                meta: Some(meta(&[("origin", &filler)])),
            }],
            meta: None,
        }),
        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate {
            current_mode_id: filler.clone(),
            meta: Some(meta(&[("note", &filler)])),
        }),
        SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate {
            config_options: vec![json!({ "id": filler.clone() })],
            meta: None,
        }),
        SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
            title: Some(filler.clone()),
            updated_at: None,
            meta: None,
        }),
        SessionUpdate::UsageUpdate(UsageUpdate {
            used: 1,
            size: 2,
            meta: Some(meta(&[("window", &filler)])),
        }),
    ];

    for update in &updates {
        assert!(
            update.estimated_bytes() >= FILLER_BYTES,
            "{update:?} holds text a holder must account for"
        );
    }
}

/// One event per variant, each holding the same filler. A variant absent from
/// the walk reports nothing no matter what it carries, so a holder that keeps
/// a window of events never reaches its byte budget on those.
#[test]
fn test_event_carries_payload() {
    let filler = "p".repeat(FILLER_BYTES);
    let events = vec![
        FrontendEvent::Message {
            delta: filler.clone(),
        },
        FrontendEvent::Diff {
            path: filler.clone(),
            patch: filler.clone(),
        },
        FrontendEvent::ToolProgress {
            name: filler.clone(),
            status: filler.clone(),
        },
        FrontendEvent::PermissionAsk {
            reason: filler.clone(),
        },
        FrontendEvent::AgentEvent {
            topic: filler.clone(),
            summary: filler.clone(),
        },
        FrontendEvent::Artifact {
            kind: filler.clone(),
            id: filler.clone(),
        },
        FrontendEvent::SpecImplDivergence {
            clause_id: filler.clone(),
            status: filler.clone(),
        },
        FrontendEvent::FindingArrived {
            finding_id: filler.clone(),
        },
        FrontendEvent::SessionUpdate {
            update: SessionUpdate::CurrentModeUpdate(CurrentModeUpdate {
                current_mode_id: filler.clone(),
                meta: None,
            }),
        },
        FrontendEvent::Acpx {
            notification: AcpxNotification::new(
                AcpxMethod::LlmTextDelta,
                json!({ "text": filler.clone() }),
            ),
        },
        FrontendEvent::QueuedInputCommitted {
            inputs: vec![QueuedInput::new(filler.clone())],
        },
        FrontendEvent::MemoryChanged {
            id: MemoryChangeId(filler.clone()),
            origin: MemoryChangeOrigin::AutoMemory,
            causality: MemoryChangeCausality::ThisTurn,
            changes: vec![MemoryChange {
                key: filler.clone(),
                operation: MemoryOperation::Created,
                scope: MemoryChangeScope::User,
            }],
        },
        FrontendEvent::SystemLine {
            text: filler.clone(),
        },
        FrontendEvent::AgentStatus {
            agent_id: filler.clone(),
            subagent_type: filler.clone(),
            turn: 1,
            tokens: 0,
            tool_uses: 0,
            last_activity: Some(filler.clone()),
            completed: None,
        },
    ];

    for event in &events {
        assert!(
            event.estimated_bytes() >= FILLER_BYTES,
            "{event:?} holds text a holder must account for"
        );
    }

    let counters = FrontendEvent::Metrics {
        tokens: 4096,
        cache_hit_ratio: 0.5,
    };
    assert_eq!(
        counters.estimated_bytes(),
        0,
        "a frame of counters carries no payload of its own"
    );
}

/// A tool's arguments are JSON the producer chose, so they nest: an edit list
/// holding the text of each edit is the ordinary shape. Every other field here
/// is empty, so the nested text is the only place the figure can come from,
/// and a walk that stops at the top level counts none of it.
#[test]
fn test_nested_arguments_count() {
    let filler = "p".repeat(FILLER_BYTES);
    let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        ToolCallId("call-1".to_string()),
        ToolCallUpdateFields {
            status: None,
            title: None,
            raw_input: Some(json!({ "edits": [{ "path": filler }] })),
            raw_output: None,
        },
    ));

    assert!(
        update.estimated_bytes() >= FILLER_BYTES,
        "nested arguments are resident"
    );
}

/// A command's advertised input carries metadata of its own, which a walk that
/// reads only the hint string counts as nothing. Every other field here is
/// empty, so the metadata is the only text the figure can come from.
#[test]
fn test_command_input_meta_counts() {
    let filler = "p".repeat(FILLER_BYTES);
    let update = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate {
        available_commands: vec![AvailableCommand {
            name: String::new(),
            description: String::new(),
            input: Some(AvailableCommandInput {
                hint: String::new(),
                meta: Some(meta(&[("arg", &filler)])),
            }),
            meta: None,
        }],
        meta: None,
    });

    assert!(
        update.estimated_bytes() >= FILLER_BYTES,
        "a command input's metadata is resident"
    );
}

/// A notification's parameters are JSON the producer chose, so the walk
/// measures whatever it put there rather than a fixed figure.
#[test]
fn test_notification_carries_payload() {
    let note = AcpxNotification::new(
        AcpxMethod::LlmTextDelta,
        json!({ "summary": "p".repeat(FILLER_BYTES) }),
    );

    assert!(
        note.estimated_bytes() >= FILLER_BYTES,
        "a notification's parameters are resident"
    );
}
