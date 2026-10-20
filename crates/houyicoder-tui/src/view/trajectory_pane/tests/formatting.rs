//! Tests for what the trajectory pane says: the spans, counts, and units a
//! rendered line states.

use super::super::list;
use super::super::*;
use super::fixtures::turn;
use super::fixtures::{detail_of, lines_text, record_of, view_of};
use crate::composition::app;
use unicode_width::UnicodeWidthStr;

/// The session total reads as a duration: twelve days of wall time is a span,
/// not a six-figure second count.
#[test]
fn test_header_total_is_span() {
    let mut view = view_of(vec![turn(1, "ask")]);
    view.duration_ms = Some(1_032_337_000);
    let (header, _, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 120, 25));
    let text = lines_text(&header);
    assert!(text.contains("total 11d 22h"), "{text}");
    assert!(!text.contains("1032337"), "no bare second count: {text}");
}

/// A failure count agrees with its number, in the header and in the row.
#[test]
fn test_fail_count_agrees() {
    let mut view = view_of(vec![turn(1, "ask")]);
    view.failures = 1;
    let (header, _, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 120, 25));
    let text = lines_text(&header);
    assert!(text.contains("1 fail"), "{text}");
    assert!(!text.contains("1 fails"), "{text}");

    view.failures = 19;
    let (header, _, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 120, 25));
    let text = lines_text(&header);
    assert!(text.contains("19 fails"), "{text}");

    if let TrajectoryRow::Turn(turn) = &mut view.rows[0] {
        turn.tool_count = 1;
        turn.tool_fail = 1;
    }
    let (_, body, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 200, 25));
    let text = lines_text(&body);
    assert!(text.contains("1 fail"), "the row agrees too: {text}");
    assert!(!text.contains("1 fails"), "{text}");
}

/// A record whose span was never measured states no latency, and one whose
/// turn offset was never measured states no offset: a 0ms line would claim a
/// measurement the log does not have.
#[test]
fn test_detail_omits_unmeasured_span() {
    let mut record = record_of(TrajectoryRecordKind::Context, None);
    record.duration_ms = 0;
    let unmeasured = turn(1, "ask");
    let detail = detail_of(record);
    let (_, body, _, _) = record_detail::draw_event_detail(&unmeasured, &detail, 0, Rect::ZERO, 0);
    let text = lines_text(&body);
    assert!(!text.contains("latency"), "no unmeasured latency: {text}");
    assert!(
        !text.contains("start"),
        "and no unmeasured offset read as zero: {text}"
    );

    // A record that was measured states both: the span it took, and how far
    // into the turn it began.
    let mut measured_record = record_of(TrajectoryRecordKind::Tool, None);
    measured_record.start_ms = 1_310;
    let detail = detail_of(measured_record);
    let (_, body, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::ZERO, 0);
    let text = lines_text(&body);
    assert!(text.contains("latency: 10ms"), "a measured one is: {text}");
    assert!(
        text.contains("start: 1.3s"),
        "and the offset reads as a span: {text}"
    );
}

/// Reasoning tokens are named as a component of the output, with their unit: a
/// bare count reads as something the pane did not name.
#[test]
fn test_detail_names_reasoning() {
    let mut record = record_of(TrajectoryRecordKind::Model, None);
    record.usage = Some(EventUsage {
        input: Some(1200),
        output: Some(340),
        cache_read: None,
        cache_write: None,
        reasoning: Some(107),
        provenance: UsageProvenance::ALL_REPORTED,
    });
    let turn = turn(1, "ask");
    let detail = detail_of(record);
    let (_, body, _, _) = record_detail::draw_event_detail(&turn, &detail, 0, Rect::ZERO, 0);
    let text = lines_text(&body);
    assert!(
        text.contains("reasoning: 107 tokens (part of output)"),
        "{text}"
    );
}

/// The detail's content fields are separate blocks: running the thinking, the
/// input, and the output together makes the reader find the boundary.
#[test]
fn test_detail_separates_fields() {
    let mut record = record_of(TrajectoryRecordKind::Tool, Some("done"));
    record.thinking = Some("a thought".into());
    record.input = Some("a command".into());
    let turn = turn(1, "ask");
    let detail = detail_of(record);
    let (_, body, _, _) = record_detail::draw_event_detail(&turn, &detail, 0, Rect::ZERO, 0);
    let lines: Vec<String> = body
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    let thinking = lines
        .iter()
        .position(|l| l.contains("thinking:"))
        .expect("a thinking block");
    let input = lines
        .iter()
        .position(|l| l.contains("input:"))
        .expect("an input block");
    let output = lines
        .iter()
        .position(|l| l.contains("output:"))
        .expect("an output block");
    let latency = lines
        .iter()
        .position(|l| l.contains("latency:"))
        .expect("the record's own span");
    assert!(lines[input + 1].trim().is_empty(), "{lines:?}");
    assert!(lines[thinking + 1].trim().is_empty(), "{lines:?}");
    assert_eq!(
        (input, latency, thinking, output),
        (0, 2, 4, 6),
        "one blank line between the blocks, in the order the record reads: \
         what it acted on, what it measured, then the thinking, then what it \
         answered: {lines:?}"
    );
}

/// A record's measurements come before its long prose: a hundred-line reply
/// ahead of them would push the latency and the usage off the pane, and the
/// reader would scroll past the text to reach the numbers it is read for.
#[test]
fn test_detail_timing_first() {
    let mut record = record_of(TrajectoryRecordKind::Model, Some("the whole reply"));
    record.duration_ms = 620;
    record.thinking = Some("a thought".into());
    record.timing = Some(EventTiming {
        total_ms: 620,
        ttft_ms: Some(210),
        decode_ms: Some(410),
        reasoning_ms: None,
        response_ms: None,
    });
    let detail = detail_of(record);
    let (_, body, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::ZERO, 0);
    let lines: Vec<String> = body
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    let timing = lines
        .iter()
        .position(|l| l.contains("timing:"))
        .expect("a timing line");
    let thinking = lines
        .iter()
        .position(|l| l.contains("thinking:"))
        .expect("a thinking block");
    let output = lines
        .iter()
        .position(|l| l.contains("output:"))
        .expect("an output block");
    let latency = lines
        .iter()
        .position(|l| l.contains("latency:"))
        .expect("the record's own span");
    assert!(
        timing < latency && latency < thinking && latency < output,
        "the measurements stay above the prose, the record's own span with \
         them: {lines:?}"
    );
}

/// The detail's header states no span the log never measured, just as its body
/// does not.
#[test]
fn test_detail_header_omits_span() {
    let mut record = record_of(TrajectoryRecordKind::Context, None);
    record.duration_ms = 0;
    let unmeasured = turn(1, "ask");
    let detail = detail_of(record);
    let (header, _, _, _) =
        record_detail::draw_event_detail(&unmeasured, &detail, 0, Rect::ZERO, 0);
    let text = lines_text(&header);
    assert!(
        !text.contains("0ms"),
        "no unmeasured span in the header: {text}"
    );

    let measured = turn(1, "ask");
    let detail = detail_of(record_of(TrajectoryRecordKind::Tool, None));
    let (header, _, _, _) = record_detail::draw_event_detail(&measured, &detail, 0, Rect::ZERO, 0);
    let text = lines_text(&header);
    assert!(text.contains("10ms"), "a measured one is stated: {text}");
}

/// A model call that measured its reasoning and output blocks says so, and one
/// that did not stays silent about them: an unmeasured span is not a zero.
#[test]
fn test_detail_shows_reasoning_split() {
    let mut measured = record_of(TrajectoryRecordKind::Model, Some("answer"));
    measured.timing = Some(EventTiming {
        total_ms: 9_000,
        ttft_ms: Some(400),
        decode_ms: Some(8_000),
        reasoning_ms: Some(3_000),
        response_ms: Some(5_500),
    });
    let detail = detail_of(measured);
    let (_, body, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::ZERO, 0);
    let text = lines_text(&body);
    assert!(text.contains("reasoning 3.0s"), "the thinking time: {text}");
    assert!(
        text.contains("generating 5.5s"),
        "and the time it spent producing output: {text}"
    );

    let mut unmeasured = record_of(TrajectoryRecordKind::Model, Some("answer"));
    unmeasured.timing = Some(EventTiming {
        total_ms: 9_000,
        ttft_ms: Some(400),
        decode_ms: Some(8_000),
        reasoning_ms: None,
        response_ms: None,
    });
    let detail = detail_of(unmeasured);
    let (_, body, _, _) =
        record_detail::draw_event_detail(&turn(1, "ask"), &detail, 0, Rect::ZERO, 0);
    let text = lines_text(&body);
    assert!(
        !text.contains("reasoning") && !text.contains("generating"),
        "an unmeasured split is not printed: {text}"
    );
}

/// A logical line wider than the pane wraps instead of being cut off at its
/// right edge, and the rows it wraps into line up under the value column, so a
/// second row does not read as another field.
#[test]
fn test_detail_wraps_long_field() {
    let mut record = record_of(TrajectoryRecordKind::Tool, None);
    record.output = Some("the value keeps going and going ".repeat(8));
    record.duration_ms = 0;
    let detail = detail_of(record);
    let width = 80;
    let (_, body, _, _) = record_detail::draw_event_detail(
        &turn(1, "ask"),
        &detail,
        0,
        Rect::new(0, 0, width, 20),
        0,
    );
    let rows: Vec<String> = body
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    let first = rows
        .iter()
        .position(|r| r.starts_with(" output: "))
        .expect("an output block");
    let labeled = rows.iter().filter(|r| r.starts_with(" output: ")).count();
    assert_eq!(labeled, 1, "the field is labeled once: {rows:?}");
    let continuation: Vec<&String> = rows[first + 1..]
        .iter()
        .take_while(|r| !r.trim().is_empty())
        .collect();
    assert!(!continuation.is_empty(), "the value wraps: {rows:?}");
    for row in &continuation {
        assert!(
            row.starts_with(&" ".repeat(9)),
            "a continuation lines up under the value column: {rows:?}"
        );
    }
    for row in &rows {
        assert!(
            UnicodeWidthStr::width(row.as_str()) <= width as usize,
            "no row runs past the pane it is drawn in: {rows:?}"
        );
    }
}

/// One span reads the same at both record levels: a reader who met 4.2s on the
/// timeline row does not meet a bare 4200ms one Enter later.
#[test]
fn test_detail_span_matches_row() {
    let mut record = record_of(TrajectoryRecordKind::Model, Some("answer"));
    record.duration_ms = 4_200;
    record.timing = Some(EventTiming {
        total_ms: 4_200,
        ttft_ms: Some(1_200),
        decode_ms: Some(3_800),
        reasoning_ms: Some(2_100),
        response_ms: None,
    });
    let turn = turn(1, "ask");
    let detail = detail_of(record);
    let app = app();
    let area = Rect::new(0, 0, 140, 20);
    let l1 = lines_text(&detail::draw_turn_detail(&turn, &detail, 0, area, &app).1);
    let l2 = lines_text(&record_detail::draw_event_detail(&turn, &detail, 0, area, 0).1);
    for reading in ["4.2s", "1.2s", "2.1s"] {
        assert!(
            l1.contains(reading),
            "the timeline row reads {reading}: {l1}"
        );
        assert!(l2.contains(reading), "and the record body agrees: {l2}");
    }
}
