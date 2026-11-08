//! Tests for what a folded turn says about the block store: the blocks a
//! result was kept in, the form the request carried, and the reads that
//! opened it.

use super::super::view::*;
use super::view::ev;
use houyicoder_context::{
    CachedModelView, CachedRead, ReadActor, ReadOutcome, SessionEvent, SessionLogEntry, ViewForm,
};
use houyicoder_tui::view::trajectory_pane::{
    BlockForm, ReadActor as ShownActor, StorageFact, TrajectoryRecordKind,
};
use serde_json::json;

const BLOCK: &str = "a1b2c3d4e5f60718";

/// A capture envelope as the bash tool publishes it: the model was handed a
/// marker for the whole output, and both streams were kept as their own
/// blocks.
fn kept_envelope() -> serde_json::Value {
    json!({
        "stdout": {
            "block_ref": BLOCK,
            "bytes": 23_040u64,
            "preview": "running 2432 tests"
        },
        "stderr": "",
        "exit_code": 0,
        "raw_capture": {
            "record": { "block_ref": "0f0f0f0f0f0f0f0f" },
            "stdout_bytes": 100_000u64,
            "stderr_bytes": 2_048u64,
            "stdout": { "block_ref": "1111111111111111" },
            "stderr": { "block_ref": "2222222222222222" }
        }
    })
}

/// Events of one turn: a prompt, the tool call, and the given result.
fn turn_with_result(output: serde_json::Value) -> Vec<SessionLogEntry> {
    vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "run the suite".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            110,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: json!({ "command": "cargo test" }),
            },
        ),
        ev(
            150,
            SessionEvent::ToolResult {
                call_id: "c1".into(),
                output,
                duration_ms: 40,
            },
        ),
    ]
}

fn storage_of(events: &[SessionLogEntry]) -> StorageFact {
    records_of(events, 1)
        .into_iter()
        .find_map(|record| record.storage)
        .expect("the kept result states its storage")
}

/// A result the host kept reports the block the model view was given and the
/// raw bytes the capture held, so a reader can see what was externalized and
/// how much of it.
#[test]
fn test_result_reports_kept_bytes() {
    let stored = storage_of(&turn_with_result(kept_envelope()));
    // Both stream lengths are recorded by the same capture, so the total is
    // the sum and not one stream standing for it.
    assert_eq!(stored.raw_bytes, Some(102_048));
    let block = stored.block.expect("the model view names its block");
    assert_eq!(block.hash, BLOCK);
    assert_eq!(block.bytes, 23_040);
    assert_eq!(block.short(), "a1b2c3d4");
    assert!(!stored.save_failed);
    assert_eq!(stored.stopped, None);
}

/// One stream length alone is a floor on what was kept, not the total, so the
/// raw size stays unknown while the block it was kept in is still stated.
#[test]
fn test_raw_bytes_need_both() {
    let mut envelope = kept_envelope();
    envelope["raw_capture"]
        .as_object_mut()
        .expect("capture object")
        .remove("stderr_bytes");
    let stored = storage_of(&turn_with_result(envelope));
    assert_eq!(stored.raw_bytes, None);
    assert_eq!(
        stored.block.expect("the block is stated").bytes,
        23_040,
        "a missing stderr length says nothing about the block"
    );
}

/// A capture that refused to publish says so, with the reason that stopped
/// it, instead of reading as an ordinary result.
#[test]
fn test_save_failure_recorded() {
    let envelope = json!({
        "stdout": "bounded tail",
        "exit_code": 0,
        "raw_capture": {
            "capture_failed": "the raw output above the bound was not kept",
            "stop_reason": "wall_budget"
        }
    });
    let stored = storage_of(&turn_with_result(envelope));
    assert!(stored.save_failed);
    assert_eq!(stored.stopped.as_deref(), Some("wall budget reached"));
    assert!(stored.block.is_none());
}

/// An assembly landing after the result attaches the form it carried to the
/// block it named, with the assembly it belonged to and when it was made.
#[test]
fn test_assembly_sets_view_form() {
    let mut events = turn_with_result(kept_envelope());
    events.push(ev(
        160,
        SessionEvent::CasModelView {
            assembly_id: 7,
            views: vec![CachedModelView {
                block_ref: BLOCK.into(),
                form: ViewForm::Summarize,
                size: 23_040,
            }],
            estimate_unit: "bytes".into(),
        },
    ));
    let stored = storage_of(&events);
    assert_eq!(stored.form, Some(BlockForm::Summarize));
    assert_eq!(stored.assembly_id, Some(7));
    assert_eq!(stored.assembly_at_ms, Some(160));
}

/// A read lands before the result that answers it, so its row is completed
/// when the turn closes: it then names the block of the stream it opened.
#[test]
fn test_read_names_its_block() {
    let mut events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "read it back".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            120,
            SessionEvent::CasRead {
                reads: vec![CachedRead {
                    result_id: "c1:stdout".into(),
                    actor: ReadActor::Model,
                    start: 0,
                    bytes: 4_096,
                    outcome: ReadOutcome::Read,
                }],
            },
        ),
    ];
    events.extend(turn_with_result(kept_envelope()).into_iter().skip(1));
    let records = records_of(&events, 1);
    let read = records
        .iter()
        .find(|record| record.kind == TrajectoryRecordKind::Storage)
        .expect("the read is its own row");
    // The stream's own block, not the whole-output marker: a read opens one
    // stream of a result.
    assert_eq!(read.summary, "read CAS 11111111 (4.0 KiB)");
    let stored = read.storage.as_ref().expect("the row states the read");
    assert_eq!(stored.reads[0].actor, ShownActor::Model);
    assert_eq!(stored.reads[0].start, 0);
    // The result the read answered now counts it.
    let result = records
        .iter()
        .find(|record| record.kind == TrajectoryRecordKind::Tool)
        .and_then(|record| record.storage.as_ref())
        .expect("the result keeps its reads");
    assert_eq!(result.reads.len(), 1);
    assert_eq!(result.reads[0].result_id, "c1:stdout");
}

/// A read whose result this turn never stored names the result it addressed
/// and keeps its outcome, rather than claiming a block it cannot know.
#[test]
fn test_unstored_read_names_result() {
    let events = vec![
        ev(
            100,
            SessionEvent::UserInput {
                text: "read the last one".into(),
            },
        ),
        ev(
            105,
            SessionEvent::TurnStarted {
                turn: 1,
                call_in_turn: 0,
            },
        ),
        ev(
            120,
            SessionEvent::CasRead {
                reads: vec![CachedRead {
                    result_id: "older:stdout".into(),
                    actor: ReadActor::Ui,
                    start: 4_096,
                    bytes: 2_048,
                    outcome: ReadOutcome::Read,
                }],
            },
        ),
    ];
    let read = records_of(&events, 1)
        .into_iter()
        .find(|record| record.kind == TrajectoryRecordKind::Storage)
        .expect("the read is its own row");
    assert_eq!(read.summary, "read older:stdout from 4096 (2.0 KiB)");
}

/// TurnBuilder must clear storage-indexed offsets between turns so a short
/// second turn does not panic accessing an index recorded during an earlier,
/// longer turn.
#[test]
fn test_storage_resets_turns() {
    let mut events = Vec::new();
    events.push(ev(
        100,
        SessionEvent::UserInput {
            text: "first long turn".into(),
        },
    ));
    events.push(ev(
        101,
        SessionEvent::TurnStarted {
            turn: 1,
            call_in_turn: 0,
        },
    ));
    for i in 1..=4 {
        events.push(ev(
            110 + i,
            SessionEvent::ToolCall {
                call_id: format!("c{i}"),
                tool: "read".into(),
                input: json!({ "path": "file.rs" }),
            },
        ));
        events.push(ev(
            120 + i,
            SessionEvent::ToolResult {
                call_id: format!("c{i}"),
                output: json!({ "lines": 10 }),
                duration_ms: 5,
            },
        ));
    }
    events.push(ev(
        130,
        SessionEvent::ToolCall {
            call_id: "c5".into(),
            tool: "bash".into(),
            input: json!({ "command": "cargo test" }),
        },
    ));
    events.push(ev(
        135,
        SessionEvent::ToolResult {
            call_id: "c5".into(),
            output: kept_envelope(),
            duration_ms: 50,
        },
    ));

    events.push(ev(
        200,
        SessionEvent::UserInput {
            text: "short turn".into(),
        },
    ));
    events.push(ev(
        205,
        SessionEvent::TurnStarted {
            turn: 2,
            call_in_turn: 0,
        },
    ));
    events.push(ev(
        210,
        SessionEvent::AssistantMessage {
            text: "done with short turn".into(),
            thinking: None,
        },
    ));

    events.push(ev(
        300,
        SessionEvent::UserInput {
            text: "third turn".into(),
        },
    ));

    let view = build_view(&events, "test", 0);
    assert_eq!(view.total_turns, 3);
}
