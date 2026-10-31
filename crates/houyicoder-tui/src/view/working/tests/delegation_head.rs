//! The delegation head row: the fold chord survives a long summary.

use super::*;
use crate::records::TranscriptLine;

/// A long summary must not push the fold chord past the row. The chord is the
/// only sign the row expands, and a terminal clips the end of a line, so a
/// summary that fills the row used to leave the reader with half a chord.
#[test]
fn test_long_summary_keeps_hint() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    let summary = "a summary long enough to fill the whole row and then keep \
                   going past the right edge of any sensible terminal width"
        .to_string();
    app.sessions.current.transcript = vec![TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "verify".into(),
        summary,
        prompt: "do the work".into(),
        folded_transcript: Vec::new(),
        color: None,
    }]
    .into();
    let out = render_text(&app, 60, 24);
    let head = out
        .lines()
        .find(|line| line.contains("verify"))
        .unwrap_or_else(|| panic!("no delegation head:\n{out}"));
    assert!(
        head.contains("(ctrl+o to expand)"),
        "the whole chord is on the row: {head:?}"
    );
    assert!(
        head.chars().count() <= 60,
        "and the row fits the width: {} columns in {head:?}",
        head.chars().count()
    );
}

/// A short summary is left exactly as written, so fitting never rewrites text
/// the row had room for.
#[test]
fn test_short_summary_unchanged() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "verify".into(),
        summary: "run the gate".into(),
        prompt: "do the work".into(),
        folded_transcript: Vec::new(),
        color: None,
    }]
    .into();
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("run the gate"),
        "a summary that fits is not truncated:\n{out}"
    );
    assert!(!out.contains('\u{2026}'), "and gains no ellipsis:\n{out}");
}

/// The hint survives every width, however little room the summary leaves it.
/// The chord is the row's only affordance, so half of it tells the reader
/// nothing: the summary gives way instead, down to an ellipsis.
#[test]
fn test_hint_survives_narrow_row() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "verify".into(),
        summary: "audit the permission decision pipeline".into(),
        prompt: "do the work".into(),
        folded_transcript: Vec::new(),
        color: None,
    }]
    .into();
    let out = render_text(&app, 30, 24);
    let head = out
        .lines()
        .find(|line| line.contains("verify"))
        .unwrap_or_else(|| panic!("no delegation head:\n{out}"));
    assert!(
        head.contains("(ctrl+o to expand)"),
        "the whole chord is drawn on a thirty-column row: {head:?}"
    );
}

/// A chord that appears is never cut. Sweeping the widths covers the break
/// between "the summary still fits" and "the summary has given everything it
/// can", which is where a partial chord would surface.
#[test]
fn test_chord_never_truncated() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    for width in [20u16, 24, 28, 30, 36, 44, 52, 60, 80, 100] {
        app.sessions.current.transcript = vec![TranscriptLine::Subagent {
            child_sid: "child-1".into(),
            subagent_type: "verify".into(),
            summary: "a summary that fills any of these widths and then some".into(),
            prompt: "do the work".into(),
            folded_transcript: Vec::new(),
            color: None,
        }]
        .into();
        let out = render_text(&app, width, 24);
        let head = out
            .lines()
            .find(|line| line.contains("verify"))
            .unwrap_or_else(|| panic!("no delegation head at {width}:\n{out}"));
        if head.contains("ctrl+") {
            assert!(
                head.contains("(ctrl+o to expand)") || head.contains("(ctrl+o to collapse)"),
                "a chord that appears is whole at width {width}: {head:?}"
            );
        }
    }
}

/// An expanded row says collapse, so the chord names the act it performs. The
/// hint is fitted the same way either way round, which is what keeps the two
/// states from needing separate truncation rules.
#[test]
fn test_expanded_row_names_collapse() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "verify".into(),
        summary: "a summary long enough to need fitting on a narrow row here".into(),
        prompt: "do the work".into(),
        folded_transcript: Vec::new(),
        color: None,
    }]
    .into();
    app.ui.transcript_view.toggle_subagent("child-1".into());
    let out = render_text(&app, 44, 24);
    let head = out
        .lines()
        .find(|line| line.contains("verify"))
        .unwrap_or_else(|| panic!("no delegation head:\n{out}"));
    assert!(
        head.contains("(ctrl+o to collapse)"),
        "the expanded row names the key that closes it: {head:?}"
    );
}

/// A summary in a wide-glyph script takes two columns per character. A fit
/// that counts characters rather than display columns therefore undercounts by
/// half, and on a CJK summary the chord is pushed off the right edge again --
/// exactly the defect this row was fixed for, still reachable through the
/// input the product's own users type.
#[test]
fn test_wide_summary_still_fits() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "verify".into(),
        summary: "审查审批裁决流水线的权限边界与回退路径是否完整覆盖".into(),
        prompt: "do the work".into(),
        folded_transcript: Vec::new(),
        color: None,
    }]
    .into();
    let out = render_text(&app, 60, 24);
    let head = out
        .lines()
        .find(|line| line.contains("verify"))
        .unwrap_or_else(|| panic!("no delegation head:\n{out}"));
    assert!(
        head.contains("(ctrl+o to expand)"),
        "the chord survives a wide-glyph summary: {head:?}"
    );
}

/// A nested delegation carries no hint, so its summary gets the room the
/// chord would have taken. Charging an empty hint a cost would shorten a
/// summary that had space, and the nested row is the one place the row can be
/// spent wholly on text.
///
/// The summary is chosen to split the two rows apart at this width: forty
/// characters fit the hint-less row and not the hinted one, so the assertion
/// fails if the two ever get the same budget.
#[test]
fn test_nested_row_full_width() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    // Expanding the outer row is what makes its folded child nested: a nested
    // delegation is shown but not operated, so it carries no hint.
    app.ui.transcript_view.toggle_subagent("child-1".into());
    let summary = "abcdefghijklmnopqrstuvwxyz0123456789ABCD";
    app.sessions.current.transcript = vec![TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "verify".into(),
        summary: summary.into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Subagent {
            child_sid: "grandchild".into(),
            subagent_type: "explore".into(),
            summary: summary.into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        }],
        color: None,
    }]
    .into();
    let out = render_text(&app, 60, 24);
    let row = |kind: &str| -> String {
        out.lines()
            .find(|line| line.contains(kind))
            .unwrap_or_else(|| panic!("no {kind} row:\n{out}"))
            .to_string()
    };
    let outer = row("verify");
    let nested = row("explore");

    assert!(
        !outer.contains(summary),
        "the hinted row has to shorten the summary: {outer:?}"
    );
    assert!(
        nested.contains(summary),
        "the hint-less row keeps the summary whole: {nested:?}"
    );
    assert!(
        !nested.contains("ctrl+o"),
        "and offers no chord it cannot honour: {nested:?}"
    );
}

/// A very long type name plus a hint still cannot overflow the row, and the
/// type is not truncated away: a row that dropped it would stop saying which
/// kind of child ran.
#[test]
fn test_long_type_keeps_identity() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.sessions.current.transcript = vec![TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "a-very-long-subagent-type-name-here".into(),
        summary: "work".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    }]
    .into();
    let out = render_text(&app, 90, 24);
    let head = out
        .lines()
        .find(|line| line.contains("a-very-long"))
        .unwrap_or_else(|| panic!("no head:\n{out}"));
    assert!(
        head.contains("(ctrl+o to expand)"),
        "the chord fits alongside a long type: {head:?}"
    );
}
