//! Tests for what the trajectory pane says: the spans, counts, and units a
//! rendered line states.

use super::super::list;
use super::super::*;
use super::{detail_view, lines_text, record_of};

/// The session total reads as a duration: twelve days of wall time is a span,
/// not a six-figure second count.
#[test]
fn test_header_total_is_span() {
    let mut view = detail_view(record_of(TrajectoryRecordKind::Tool, None));
    view.duration_secs = 1_032_337;
    let (header, _, _, _) = list::draw_turn_list(&view, 0, Rect::new(0, 0, 120, 25));
    let text = lines_text(&header);
    assert!(text.contains("total 11d 22h"), "{text}");
    assert!(!text.contains("1032337"), "no bare second count: {text}");
}

/// A failure count agrees with its number, in the header and in the row.
#[test]
fn test_fail_count_agrees() {
    let mut view = detail_view(record_of(TrajectoryRecordKind::Tool, None));
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

/// A record whose span was never measured states no latency: a 0ms line would
/// claim a measurement the log does not have.
#[test]
fn test_detail_omits_unmeasured_latency() {
    let mut record = record_of(TrajectoryRecordKind::Context, None);
    record.duration_ms = 0;
    let view = detail_view(record);
    let (_, body, _, _) = detail::draw_event_detail(&view, 0, 0, Rect::ZERO);
    let text = lines_text(&body);
    assert!(!text.contains("latency"), "no unmeasured latency: {text}");
    assert!(
        text.contains("start: 0ms"),
        "the offset is still shown: {text}"
    );

    let view = detail_view(record_of(TrajectoryRecordKind::Tool, None));
    let (_, body, _, _) = detail::draw_event_detail(&view, 0, 0, Rect::ZERO);
    let text = lines_text(&body);
    assert!(text.contains("latency: 10ms"), "a measured one is: {text}");
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
    });
    let view = detail_view(record);
    let (_, body, _, _) = detail::draw_event_detail(&view, 0, 0, Rect::ZERO);
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
    let view = detail_view(record);
    let (_, body, _, _) = detail::draw_event_detail(&view, 0, 0, Rect::ZERO);
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
    assert_eq!(input, thinking + 2, "one blank line between: {lines:?}");
    assert!(lines[thinking + 1].trim().is_empty(), "{lines:?}");
}

/// The detail's header states no span the log never measured, just as its body
/// does not.
#[test]
fn test_detail_header_omits_span() {
    let mut record = record_of(TrajectoryRecordKind::Context, None);
    record.duration_ms = 0;
    let view = detail_view(record);
    let (header, _, _, _) = detail::draw_event_detail(&view, 0, 0, Rect::ZERO);
    let text = lines_text(&header);
    assert!(
        !text.contains("0ms"),
        "no unmeasured span in the header: {text}"
    );

    let view = detail_view(record_of(TrajectoryRecordKind::Tool, None));
    let (header, _, _, _) = detail::draw_event_detail(&view, 0, 0, Rect::ZERO);
    let text = lines_text(&header);
    assert!(text.contains("10ms"), "a measured one is stated: {text}");
}
