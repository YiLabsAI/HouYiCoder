//! Tests for the level 1 timeline's columns: where the bars are drawn and
//! what the ruler above them says.

use super::super::*;
use super::fixtures::{detail_of, detail_of_all, lines_text, record_of, turn, view};
use crate::composition::app;
use std::slice::from_ref;

/// The level 1 timeline as rendered: the ruler line, a row's text, and the
/// display column a bar starts at.
fn timeline_text(record: TrajectoryRecord) -> (String, String) {
    let mut turn = turn(1, "ask");
    turn.duration_ms = 1000;
    let detail = detail_of(record);
    let app = app();
    let (header, body, _, _) =
        detail::draw_turn_detail(&turn, &detail, 0, Rect::new(0, 0, 140, 20), &app);
    let ruler = header
        .iter()
        .map(|l| lines_text(from_ref(l)))
        .find(|l| l.matches('·').count() > 4)
        .expect("a ruler line");
    let row = body
        .iter()
        .map(|l| lines_text(from_ref(l)))
        .find(|l| l.contains('█'))
        .expect("a bar row");
    (ruler, row)
}

/// The bar's first display column in a rendered timeline row.
fn bar_column(row: &str) -> usize {
    let start = row.find('█').expect("a bar");
    UnicodeWidthStr::width(&row[..start])
}

/// The ruler's axis spans exactly the columns the bars are drawn in: a ruler
/// offset from the bars reads as a measurement that is not there.
#[test]
fn test_timeline_ruler_aligns() {
    let mut record = record_of(TrajectoryRecordKind::Tool, None);
    record.start_ms = 0;
    record.duration_ms = 1000;
    let (ruler, row) = timeline_text(record);
    let zero = ruler.chars().take_while(|c| *c == ' ').count();
    assert_eq!(
        zero,
        bar_column(&row),
        "the ruler's zero sits at the bar's first column: {ruler:?} / {row:?}"
    );
    assert_eq!(
        ruler.chars().count(),
        zero + row.matches('█').count(),
        "and it spans the bar's columns: {ruler:?} / {row:?}"
    );
}

/// The name column counts display columns, so a wide glyph in a record's name
/// does not shift the bar or the columns after it.
#[test]
fn test_timeline_name_aligns() {
    let mut ascii = record_of(TrajectoryRecordKind::Tool, None);
    ascii.name = Some("bash".into());
    let mut wide = record_of(TrajectoryRecordKind::Tool, None);
    wide.name = Some("读文件".into());
    let (_, ascii_row) = timeline_text(ascii);
    let (_, wide_row) = timeline_text(wide);
    assert_eq!(
        bar_column(&ascii_row),
        bar_column(&wide_row),
        "a wide name does not move the bar: {ascii_row:?} / {wide_row:?}"
    );
}

/// A narrow terminal gives up the summary before the duration and the outcome:
/// every row still fits the width it is drawn in, and a measured suffix keeps
/// its own columns where there is room for them.
#[test]
fn test_timeline_fits_narrow() {
    let mut model = record_of(TrajectoryRecordKind::Model, None);
    model.timing = Some(EventTiming {
        total_ms: 620,
        ttft_ms: Some(210),
        decode_ms: Some(410),
        reasoning_ms: None,
        response_ms: None,
    });
    model.usage = Some(EventUsage {
        input: None,
        output: Some(340),
        cache_read: None,
        cache_write: None,
        reasoning: None,
        provenance: UsageProvenance::ALL_REPORTED,
    });
    let cases = [
        (record_of(TrajectoryRecordKind::Tool, None), false),
        (model, true),
    ];
    for (record, measured) in cases {
        for width in [40u16, 60, 80, 140] {
            let mut turn = turn(1, "ask");
            turn.duration_ms = 1000;
            let detail = detail_of(record.clone());
            let app = app();
            let (_, body, _, _) =
                detail::draw_turn_detail(&turn, &detail, 0, Rect::new(0, 0, width, 20), &app);
            let row = body.first().expect("a row");
            let text = lines_text(from_ref(row));
            let drawn = UnicodeWidthStr::width(text.as_str());
            assert!(
                drawn <= width as usize,
                "width {width} drew {drawn}: {text:?}"
            );
            assert!(text.contains('✓'), "the outcome survives: {text:?}");
            if measured && width >= 80 {
                assert!(
                    text.contains("tok/s"),
                    "a measured suffix keeps its columns where they fit: {text:?}"
                );
            }
        }
    }
}

/// A terminal too narrow for a time axis gets the row without one: the kind,
/// the name, the duration, and the outcome are what a row is read for, and none
/// of them may be cut off the end.
#[test]
fn test_timeline_compact_narrow() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("bash"));
    record.summary = "cargo test --workspace".into();
    record.duration_ms = 3_200;

    for width in [1usize, 2, 5, 8, 10, 19, 20, 30, 39] {
        let detail = detail_of(record.clone());
        let app = app();
        let (header, body, _, _) = detail::draw_turn_detail(
            &turn(1, "ask"),
            &detail,
            0,
            Rect::new(0, 0, width as u16, 20),
            &app,
        );
        let row = lines_text(&body);
        assert!(
            !lines_text(&header).contains("···"),
            "no ruler without a bar at {width} columns: {:?}",
            lines_text(&header)
        );
        if width >= 4 + 4 + 2 {
            assert!(
                row.contains("3.2s"),
                "the duration stays where there is room for it, {width} columns: {row}"
            );
        }
        assert!(row.contains('✓'), "and so does the outcome: {row}");
        for line in body.iter() {
            assert!(
                line.width() <= width,
                "the row fits the terminal at {width} columns: {row}"
            );
        }
        if width >= 20 {
            assert!(
                row.contains("tool"),
                "a row with room names what it was, {width} columns: {row}"
            );
        }
    }

    // At the width a bar is drawn in, the axis is back.
    let detail = detail_of(record);
    let app = app();
    let (header, body, _, _) =
        detail::draw_turn_detail(&turn(1, "ask"), &detail, 0, Rect::new(0, 0, 40, 20), &app);
    assert!(
        lines_text(&header).contains("···"),
        "a ruler at 40 columns: {:?}",
        lines_text(&header)
    );
    assert!(
        lines_text(&body).contains("bash"),
        "the name column is back with the axis: {:?}",
        lines_text(&body)
    );
}

/// A compact row keeps the selection glyph and the outcome it carries, and a
/// wide glyph in a name does not push the duration off the row.
#[test]
fn test_compact_row_keeps_marks() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("测试工具"));
    record.summary = "运行测试".into();
    record.outcome = RecordOutcome::Failed;
    record.duration_ms = 1_000;

    // Two records, so the cursor can be on the second one rather than clamping
    // onto the only row.
    let mut second = record.clone();
    second.summary = "另一件事".into();
    for cursor in [0usize, 1] {
        let detail = detail_of_all(vec![record.clone(), second.clone()]);
        let app = app();
        let (_, body, _, _) = detail::draw_turn_detail(
            &turn(1, "ask"),
            &detail,
            cursor,
            Rect::new(0, 0, 30, 20),
            &app,
        );
        let rows: Vec<String> = body.iter().map(|l| lines_text(from_ref(l))).collect();
        let marked: Vec<&String> = rows.iter().filter(|row| row.contains('▸')).collect();
        assert_eq!(marked.len(), 1, "one row carries the cursor: {rows:?}");
        assert!(
            marked[0].contains('✗'),
            "and it is the row the cursor is on: {rows:?}"
        );
        assert!(
            rows.iter()
                .all(|row| row.contains('✗') && row.contains("1.0s")),
            "every row keeps its outcome and duration: {rows:?}"
        );
        for line in body.iter() {
            assert!(line.width() <= 30, "the row fits: {rows:?}");
        }
    }
}

/// The axis stops growing at its cap: a wide terminal spends the extra columns
/// on the name and summary, so a model row keeps its wait and rate instead of
/// losing them to a bar the eye cannot read.
#[test]
fn test_timeline_bar_is_capped() {
    let mut record = record_of(TrajectoryRecordKind::Model, None);
    record.start_ms = 0;
    record.duration_ms = 1000;
    record.timing = Some(EventTiming {
        total_ms: 1000,
        ttft_ms: Some(210),
        decode_ms: Some(600),
        reasoning_ms: Some(300),
        response_ms: Some(400),
    });
    record.usage = Some(EventUsage {
        output: Some(600),
        ..EventUsage::default()
    });
    let mut turn = turn(1, "ask");
    turn.duration_ms = 1000;
    let detail = detail_of(record);
    let app = app();
    let (_, body, _, _) =
        detail::draw_turn_detail(&turn, &detail, 0, Rect::new(0, 0, 200, 20), &app);
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(
        text.matches('█').count(),
        TIMELINE_BAR_MAX_W,
        "the bar stops at its cap: {text:?}"
    );
    assert!(text.contains("TTFT 210ms"), "the wait survives: {text}");
}

/// At a common 130-column terminal a turn that used two models still shows the
/// model column; hiding it below 148 columns hid the switch itself.
#[test]
fn test_model_column_survives_narrow() {
    let turn = TrajectoryTurn {
        models: vec![
            "deepseek-v4-pro-0813".to_string(),
            "deepseek-v4.1-flash".to_string(),
        ],
        tokens_in: Some(1_200),
        tokens_out: Some(500),
        duration_ms: 100,
        ..turn(1, "switched")
    };
    let view = TrajectoryView {
        models_used: 2,
        ..view(vec![TrajectoryRow::Turn(turn)])
    };
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 130, 10));
    let text: String = body[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        text.contains("v4-pro-0813") && text.contains("+1"),
        "the switch stays visible at 130 columns: {text:?}"
    );
}

/// The switch marker leads the row and survives a reply: it is a field on the
/// record, rendered where the row is drawn, not text folded into the summary.
#[test]
fn test_row_states_switch_marker() {
    let mut record = record_of(TrajectoryRecordKind::Model, None);
    record.name = Some("deepseek-v4.1-flash".into());
    record.ordinal = 2;
    record.model_switch_from = Some("deepseek-v4-pro-0813".into());
    let mut turn = turn(1, "ask");
    turn.duration_ms = 1000;
    turn.models = vec![
        "deepseek-v4-pro-0813".to_string(),
        "deepseek-v4.1-flash".to_string(),
    ];
    let detail = detail_of(record);
    let app = app();
    let (_, body, _, _) =
        detail::draw_turn_detail(&turn, &detail, 0, Rect::new(0, 0, 140, 20), &app);
    let text: String = body
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        text.contains("[model: deepseek-v4-pro-0813 -> v4.1-flash]"),
        "the row leads with the switch: {text:?}"
    );
}
