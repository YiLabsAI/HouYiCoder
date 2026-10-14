//! The capability panes' own tests: the review verdict styling and the
//! sign-off row's buttons.

use super::*;

#[test]
fn test_review_uses_shared_verdict() {
    // The review pane delegates to the shared component, so a real verdict
    // is Cyan (monochrome), not Red.
    assert_eq!(components::verdict_style("real").fg, Some(Color::Cyan));
    assert_eq!(
        components::verdict_style("refuted").fg,
        Some(Color::DarkGray)
    );
}

#[test]
fn test_signoff_row_buttons() {
    let line = components::signoff_row(Verdict::Pending);
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(joined.contains("approve"));
    assert!(joined.contains("reject"));
}

mod permission;
