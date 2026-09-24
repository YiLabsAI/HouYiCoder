//! Tests for the level 1 timeline's columns: where the bars are drawn and
//! what the ruler above them says.

use super::super::*;
use super::fixtures::{detail_of, lines_text, record_of, turn_row};

/// The level 1 timeline as rendered: the ruler line, a row's text, and the
/// display column a bar starts at.
fn timeline_text(record: TrajectoryRecord) -> (String, String) {
    let mut row = turn_row(1, "ask");
    if let TrajectoryRow::Turn(turn) = &mut row {
        turn.duration_ms = 1000;
    }
    let detail = detail_of(record);
    let app = crate::composition::app();
    let (header, body, _, _) =
        detail::draw_turn_detail(&row, &detail, 0, Rect::new(0, 0, 140, 20), &app);
    let ruler = header
        .iter()
        .map(|l| lines_text(std::slice::from_ref(l)))
        .find(|l| l.matches('·').count() > 4)
        .expect("a ruler line");
    let row = body
        .iter()
        .map(|l| lines_text(std::slice::from_ref(l)))
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
    });
    model.usage = Some(EventUsage {
        input: None,
        output: Some(340),
        cache_read: None,
        cache_write: None,
        reasoning: None,
    });
    let cases = [
        (record_of(TrajectoryRecordKind::Tool, None), false),
        (model, true),
    ];
    for (record, measured) in cases {
        for width in [40u16, 60, 80, 140] {
            let mut row = turn_row(1, "ask");
            if let TrajectoryRow::Turn(turn) = &mut row {
                turn.duration_ms = 1000;
            }
            let detail = detail_of(record.clone());
            let app = crate::composition::app();
            let (_, body, _, _) =
                detail::draw_turn_detail(&row, &detail, 0, Rect::new(0, 0, width, 20), &app);
            let row = body.first().expect("a row");
            let text = lines_text(std::slice::from_ref(row));
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
