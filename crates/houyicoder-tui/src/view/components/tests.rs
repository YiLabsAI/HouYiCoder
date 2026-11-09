use super::*;
use crate::composition::app;

#[test]
fn test_verdict_style_colors() {
    assert_eq!(verdict_style("real").fg, Some(Color::Cyan));
    assert_eq!(verdict_style("refuted").fg, Some(Color::DarkGray));
}

#[test]
fn test_approve_row_colors() {
    let line = approve_reject_row(Verdict::Pending);
    let approved_span = line.spans.iter().find(|s| s.content.contains("approve"));
    assert!(
        approved_span.is_some_and(|s| s.style.fg == Some(Color::Cyan)),
        "pending approve should be Cyan"
    );
    let reject_span = line.spans.iter().find(|s| s.content.contains("reject"));
    assert!(
        reject_span.is_some_and(|s| s.style.fg == Some(Color::Red)),
        "pending reject should be Red"
    );
}

#[test]
fn test_signoff_row_buttons() {
    let line = signoff_row(Verdict::Pending);
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(joined.contains("approve"));
    assert!(joined.contains("reject"));
}

#[test]
fn test_consensus_line_verdicts() {
    let findings = app().workflow.review.findings.clone();
    let line = consensus_line(&findings);
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    // three lenses with check/cross marks, a weighted verdict, and a tally
    assert!(joined.contains("correctness"), "lens missing: {joined}");
    assert!(joined.contains("security"), "lens missing: {joined}");
    assert!(joined.contains("style"), "lens missing: {joined}");
    assert!(joined.contains('\u{2717}'), "cross mark missing: {joined}");
    assert!(joined.contains('\u{2713}'), "check mark missing: {joined}");
    assert!(joined.contains("(2/3)"), "tally missing: {joined}");
}

fn joined(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// A heading appears once per section, counts the rows under it, and sits
/// above them; the line the cursor stands on is the row, never the heading.
#[test]
fn test_section_list_counts() {
    let sections = ["Running", "Running", "Ended"];
    let (lines, selected) = section_list_body(&sections, 0, |index, spans| {
        spans.push(Span::raw(format!("row {index}")));
    });
    let text: Vec<String> = lines.iter().map(joined).collect();
    assert_eq!(
        text,
        vec![
            "  Running (2)",
            "\u{25b6} row 0",
            "  row 1",
            "  Ended (1)",
            "  row 2",
        ]
    );
    assert_eq!(lines[0].spans[0].style.fg, Some(Color::Cyan));
    assert_eq!(selected, 1, "the cursor line is the row, not the heading");
}

/// A cursor past the list settles on the last row, so the marker and the line
/// the caller scrolls to always name the same row.
#[test]
fn test_section_list_clamps_cursor() {
    let sections = ["Running", "Ended"];
    let (lines, selected) = section_list_body(&sections, 9, |_, spans| {
        spans.push(Span::raw("row"));
    });
    assert_eq!(selected, lines.len() - 1);
    assert!(
        joined(&lines[selected]).starts_with("\u{25b6}"),
        "the clamped cursor marks the last row"
    );
}
