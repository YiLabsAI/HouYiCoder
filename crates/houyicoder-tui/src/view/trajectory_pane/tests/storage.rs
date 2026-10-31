//! Tests for what the pane says about the block store: the tag a kept result
//! carries on its timeline row, and the four-stage account in its detail.

use super::super::*;
use super::fixtures::{detail_of, lines_text, record_of, turn};
use crate::composition::app;

/// The storage a kept result reports: a model-view block of 23 KiB out of 100
/// KiB the capture held, carried as a preview by assembly 7.
fn kept_fact() -> StorageFact {
    StorageFact {
        raw_bytes: Some(102_400),
        block: Some(StoredBlock {
            hash: "a1b2c3d4e5f60718".into(),
            bytes: 23_040,
        }),
        form: Some(BlockForm::Summarize),
        assembly_id: Some(7),
        assembly_at_ms: Some(1_700_000_000_000),
        ..StorageFact::default()
    }
}

/// A kept result leads its timeline row with the size it was stored at, so the
/// call that triggered externalization is identifiable at a glance.
#[test]
fn test_row_marks_kept_block() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("running tests"));
    record.storage = Some(kept_fact());
    let mut turn = turn(1, "run the suite");
    turn.duration_ms = 1000;
    let detail = detail_of(record);
    let app = app();
    let (_, body, _, _) =
        detail::draw_turn_detail(&turn, &detail, 0, Rect::new(0, 0, 140, 20), &app);
    let text = lines_text(&body);
    assert!(text.contains("[CAS 22.5 KiB]"), "{text}");
}

/// The tag survives the narrow form of the row, where the time axis is gone:
/// it is the one fact that says why the output does not read in full.
#[test]
fn test_narrow_row_keeps_tag() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("running tests"));
    record.storage = Some(kept_fact());
    let mut turn = turn(1, "run the suite");
    turn.duration_ms = 1000;
    let detail = detail_of(record);
    let app = app();
    let (_, body, _, _) =
        detail::draw_turn_detail(&turn, &detail, 0, Rect::new(0, 0, 50, 20), &app);
    let text = lines_text(&body);
    assert!(text.contains("[CAS 22.5 KiB]"), "{text}");
}

/// The detail states the block, the raw bytes behind it, and the form the
/// request carried, so a reader can tell what the model saw from what was
/// kept.
#[test]
fn test_detail_shows_storage() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("running tests"));
    record.storage = Some(kept_fact());
    let detail = detail_of(record);
    let (_, body, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::new(0, 0, 120, 20), 0);
    let text = lines_text(&body);
    assert!(
        text.contains("storage: CAS block a1b2c3d4 (22.5 KiB) · 100.0 KiB raw · tier: preview"),
        "{text}"
    );
}

/// The four stages stay in the header, above the output: which of capture,
/// save, model view, and read lost the output is answered without scrolling.
#[test]
fn test_header_states_four_stages() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("running tests"));
    record.storage = Some(kept_fact());
    let detail = detail_of(record);
    let (header, _, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::new(0, 0, 200, 20), 0);
    let text = lines_text(&header);
    assert!(
        text.contains("captured 100.0 KiB → saved → model view a1b2c3d4 22.5 KiB preview"),
        "{text}"
    );
    assert!(text.contains("in assembly 7"), "{text}");
    assert!(text.contains("read none this turn"), "{text}");
}

/// A capture that kept nothing still states every stage, with unknown where
/// the log has no evidence: a shorter line would read as fewer stages rather
/// than as a stage that lost the output.
#[test]
fn test_header_states_unknown() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("timed out"));
    record.storage = Some(StorageFact {
        save_failed: true,
        stopped: Some("wall budget reached".into()),
        ..StorageFact::default()
    });
    let detail = detail_of(record);
    let (header, _, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::new(0, 0, 200, 20), 0);
    let text = lines_text(&header);
    assert!(text.contains("captured unknown"), "{text}");
    assert!(text.contains("save failed"), "{text}");
    assert!(text.contains("view unknown"), "{text}");
    assert!(text.contains("read none this turn"), "{text}");
}

/// An ordinary result states no storage at all, in the row or in the detail.
#[test]
fn test_plain_result_states_none() {
    let record = record_of(TrajectoryRecordKind::Tool, Some("ok"));
    let detail = detail_of(record);
    let (header, body, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::new(0, 0, 120, 20), 0);
    assert!(!lines_text(&header).contains("storage"), "{header:?}");
    assert!(!lines_text(&body).contains("storage"), "{body:?}");
}

/// A read row states the result it addressed, the block it opened, who read
/// it, and what it delivered.
#[test]
fn test_read_row_states_facts() {
    let mut record = record_of(TrajectoryRecordKind::Storage, None);
    record.name = Some("model".into());
    record.storage = Some(StorageFact {
        reads: vec![StorageRead {
            result_id: "c1:stdout".into(),
            block: Some("a1b2c3d4e5f60718".into()),
            actor: ReadActor::Model,
            outcome: ReadOutcome::Read,
            start: 4096,
            bytes: 4096,
        }],
        ..StorageFact::default()
    });
    let detail = detail_of(record);
    let (_, body, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::new(0, 0, 160, 20), 0);
    let text = lines_text(&body);
    assert!(
        text.contains("read: c1:stdout · CAS a1b2c3d4 · by model · 4.0 KiB from byte 4096"),
        "{text}"
    );
}

/// A read that delivered nothing names no bytes, and one whose outcome the log
/// left unknown says so rather than counting it as a miss.
#[test]
fn test_read_outcomes() {
    fn body_for(outcome: ReadOutcome, start: u64) -> String {
        let mut record = record_of(TrajectoryRecordKind::Storage, None);
        record.storage = Some(StorageFact {
            reads: vec![StorageRead {
                result_id: "r1".into(),
                block: None,
                actor: ReadActor::Ui,
                outcome,
                start,
                bytes: 0,
            }],
            ..StorageFact::default()
        });
        let detail = detail_of(record);
        let (_, body, _, _) = record_detail::draw_event_detail(
            &turn(1, "ask"),
            &detail,
            0,
            Rect::new(0, 0, 160, 20),
            0,
        );
        lines_text(&body)
    }
    let failed = body_for(ReadOutcome::Failed, 0);
    assert!(failed.contains("read: r1 · by UI · failed"), "{failed}");
    let unknown = body_for(ReadOutcome::Unknown, 0);
    assert!(
        unknown.contains("read: r1 · by UI · outcome unknown"),
        "{unknown}"
    );
    let read_zero = body_for(ReadOutcome::Read, 0);
    assert!(
        read_zero.contains("read: r1 · by UI · 0 B"),
        "a read from byte zero names the size alone: {read_zero}"
    );
}

/// A block the model view carried without a recorded raw size still states the
/// save stage as unknown, so the gap is visible rather than read as saved.
#[test]
fn test_header_save_unknown() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("ok"));
    record.storage = Some(StorageFact {
        block: Some(StoredBlock {
            hash: "a1b2c3d4e5f60718".into(),
            bytes: 23_040,
        }),
        ..StorageFact::default()
    });
    let detail = detail_of(record);
    let (header, _, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::new(0, 0, 200, 20), 0);
    let text = lines_text(&header);
    assert!(text.contains("captured unknown"), "{text}");
    assert!(text.contains("save unknown"), "{text}");
}

/// A capture that stopped before the command ended states the stop in the save
/// stage, distinct from a refusal that kept nothing.
#[test]
fn test_header_states_stop() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("ok"));
    record.storage = Some(StorageFact {
        raw_bytes: Some(50_000),
        stopped: Some("wall budget reached".into()),
        reads: vec![
            StorageRead {
                result_id: "c1:stdout".into(),
                block: None,
                actor: ReadActor::Model,
                outcome: ReadOutcome::Read,
                start: 0,
                bytes: 4096,
            },
            StorageRead {
                result_id: "c1:stderr".into(),
                block: None,
                actor: ReadActor::Ui,
                outcome: ReadOutcome::Failed,
                start: 0,
                bytes: 0,
            },
        ],
        ..StorageFact::default()
    });
    let detail = detail_of(record);
    let (header, _, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::new(0, 0, 200, 20), 0);
    let text = lines_text(&header);
    assert!(
        text.contains("saved, stopped at wall budget reached"),
        "{text}"
    );
    assert!(text.contains("read 2 (model 1, UI 1)"), "{text}");
}

/// A zero assembly timestamp is not a time the log wrote, so no moment is
/// appended even when the assembly id is stated.
#[test]
fn test_assembly_zero_time_omitted() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("ok"));
    record.storage = Some(StorageFact {
        raw_bytes: Some(102_400),
        block: Some(StoredBlock {
            hash: "a1b2c3d4e5f60718".into(),
            bytes: 23_040,
        }),
        assembly_id: Some(7),
        assembly_at_ms: Some(0),
        ..StorageFact::default()
    });
    let detail = detail_of(record);
    let (header, _, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::new(0, 0, 200, 20), 0);
    let text = lines_text(&header);
    assert!(text.contains("in assembly 7"), "{text}");
    assert!(!text.contains("never"), "zero is not a time: {text}");
}

/// The store tag gives way when the column cannot hold it, rather than reading
/// as half a marker; the detail still states the block.
#[test]
fn test_tag_drops_when_narrow() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("ok"));
    record.storage = Some(kept_fact());
    let mut turn = turn(1, "ask");
    turn.duration_ms = 1000;
    let detail = detail_of(record);
    let app = app();
    let (_, body, _, _) =
        detail::draw_turn_detail(&turn, &detail, 0, Rect::new(0, 0, 30, 20), &app);
    let text = lines_text(&body);
    assert!(
        !text.contains("[CAS"),
        "the tag drops whole when it cannot fit: {text}"
    );
}
