//! The compaction row: one foldable entry instead of a raw checkpoint line
//! plus the model's whole digest pasted into the transcript.

use super::*;

/// Collapsed, a compaction is one dim line naming the event and the chord that
/// reveals the digest. The digest itself must not be on screen: it is
/// reference material, and pasting it was what filled the transcript.
#[test]
fn test_compaction_collapsed() {
    let mut app = working_app();
    app.sessions.current.transcript = vec![TranscriptLine::Compaction {
        key: "cp-1".into(),
        stats: "754 turns, 5885 events".into(),
        summary: "the model wrote a long digest\nover several lines".into(),
    }]
    .into();
    let out = render_text(&app, 100, 30);
    assert!(
        out.contains("Context compacted"),
        "the event is named: {out}"
    );
    assert!(
        out.contains("754 turns, 5885 events"),
        "the statistics are kept: {out}"
    );
    assert!(
        out.contains("ctrl+o to expand"),
        "the row offers the fold: {out}"
    );
    assert!(
        !out.contains("model wrote a long digest"),
        "the digest stays folded away: {out}"
    );
}

/// Expanded, the digest is readable and the same chord offers the close. A
/// fold that only opens is the trap the row is meant to avoid.
#[test]
fn test_compaction_expanded() {
    let mut app = working_app();
    app.sessions.current.transcript = vec![TranscriptLine::Compaction {
        key: "cp-1".into(),
        stats: "754 turns".into(),
        summary: "the model wrote a long digest".into(),
    }]
    .into();
    app.ui.transcript_view.toggle_fold("cp-1".into());
    let out = render_text(&app, 100, 30);
    assert!(
        out.contains("the model wrote a long digest"),
        "the digest is readable once expanded: {out}"
    );
    assert!(
        out.contains("ctrl+o to collapse"),
        "and the same chord closes it: {out}"
    );
}

/// A boundary that reported no statistics shows the event without an empty
/// pair of parentheses.
#[test]
fn test_compaction_no_stats() {
    let mut app = working_app();
    app.sessions.current.transcript = vec![TranscriptLine::Compaction {
        key: "cp-1".into(),
        stats: String::new(),
        summary: String::new(),
    }]
    .into();
    let out = render_text(&app, 100, 30);
    assert!(out.contains("Context compacted"), "{out}");
    assert!(
        out.contains("Context compacted (ctrl+o"),
        "the fold hint follows the event name directly, with no empty
         statistics parenthesis in between: {out}"
    );
}

/// An expanded compaction counts its digest rows, not just its head. The
/// scroll total, the search-jump offsets and the fold-aware row walk all read
/// this count, so an undercount pins the viewport past rows the reader can
/// see.
#[test]
fn test_compaction_count_tracks_digest() {
    let mut app = working_app();
    app.sessions.current.transcript = vec![TranscriptLine::Compaction {
        key: "cp-1".into(),
        stats: "900 -> 120 tokens".into(),
        summary: "one\n two\n three".into(),
    }]
    .into();
    drop(render_text(&app, 100, 30));

    let line = TranscriptLine::Compaction {
        key: "cp-1".into(),
        stats: "900 -> 120 tokens".into(),
        summary: "one\n two\n three".into(),
    };
    let collapsed = app.line_display_rows(&line);
    app.ui.transcript_view.toggle_fold("cp-1".into());
    drop(render_text(&app, 100, 30));
    let expanded = app.line_display_rows(&line);

    assert_eq!(collapsed, 1, "collapsed is the head row alone");
    assert_eq!(
        expanded, 4,
        "expanded adds one row per digest line, matching the draw path"
    );
}
