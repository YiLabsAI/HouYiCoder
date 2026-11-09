//! The compaction row: one foldable entry instead of a raw checkpoint line
//! plus the model's whole digest pasted into the transcript.

use super::*;
use crate::records::compaction_row_text;
use crate::ui::toggle_hint::ToggleHint;

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

#[test]
fn test_compaction_digest_capped() {
    let mut app = working_app();
    let mut long_summary = (0..50)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    long_summary.push_str("\nEarlier conversation context (10 turns, 20 events).\nFiles touched:\n--- derived-from-log (authoritative) ---\nDerivation: event=01A");
    let line = TranscriptLine::Compaction {
        key: "cp-cap".into(),
        stats: "900 -> 120 tokens".into(),
        summary: long_summary,
    };
    app.sessions.current.transcript = vec![line.clone()].into();
    drop(render_text(&app, 100, 50));
    assert!(
        app.toggle_tail_expand(),
        "ctrl+o tail expand reaches compaction row"
    );
    let out = render_text(&app, 100, 50);
    assert!(out.contains("line 0"), "{out}");
    assert!(out.contains("line 39"), "{out}");
    assert!(out.contains("… and more (searchable via /search)"), "{out}");
    assert_eq!(
        app.line_display_rows(&line),
        42,
        "count matches capped render (1 head + 40 lines + 1 overflow notice)"
    );
    app.toggle_fold_at_row(0);
    assert!(
        !app.ui
            .transcript_view
            .expanded
            .fold_groups
            .contains("cp-cap"),
        "clicking head row collapses compaction digest"
    );
}

/// ATTACK: two compactions in one session. The second summary must attach to
/// the second boundary, not overwrite the first one's digest.
#[test]
fn test_digests_stay_apart() {
    let mut app = working_app();
    app.sessions.current.transcript = vec![
        TranscriptLine::Compaction {
            key: "cp-1".into(),
            stats: "first".into(),
            summary: "digest one".into(),
        },
        TranscriptLine::Compaction {
            key: "cp-2".into(),
            stats: "second".into(),
            summary: "digest two".into(),
        },
    ]
    .into();
    app.ui.transcript_view.toggle_fold("cp-1".into());
    let out = render_text(&app, 100, 30);
    assert!(out.contains("digest one"), "first digest visible: {out}");
    assert!(!out.contains("digest two"), "second stays folded: {out}");
}

/// ATTACK: a summary that arrives with no boundary at all gets a row with an
/// empty fold key. The row advertises the expand chord, which the user presses,
/// and nothing can happen because no fold group owns that key. The bug-log
/// standard says an affordance must be backed by an action.
#[test]
fn test_orphan_no_chord() {
    let mut app = working_app();
    app.sessions.current.transcript = vec![TranscriptLine::Compaction {
        key: String::new(),
        stats: String::new(),
        summary: "an orphan digest".into(),
    }]
    .into();
    let out = render_text(&app, 100, 30);
    assert!(
        !out.contains("ctrl+o to expand"),
        "an orphan row must not advertise a chord it cannot honour: {out}"
    );
    assert!(
        out.contains("an orphan digest") || out.contains("Context compacted"),
        "the event is still named: {out}"
    );
}

/// ATTACK on the cross-path invariant: verbose mode draws the digest without a
/// fold, so the row count must account for it. If counts.rs forgets verbose
/// while the renderer honours it, every scroll offset after the row is wrong.
#[test]
fn test_verbose_count_matches_render() {
    let mut app = working_app();
    app.ui.verbose = true;
    let line = TranscriptLine::Compaction {
        key: "cp-1".into(),
        stats: "900 -> 120".into(),
        summary: "one\n two\n three".into(),
    };
    app.sessions.current.transcript = vec![line.clone()].into();
    let out = render_text(&app, 100, 30);
    let drawn = out.lines().filter(|l| !l.trim().is_empty()).count();
    let counted = app.line_display_rows(&line);
    assert_eq!(
        counted, 4,
        "verbose draws the head plus three digest rows: counted={counted} drawn={drawn}\n{out}"
    );
}

/// The exact label the bug report asked for: statistics and the fold chord in
/// one parenthesised pair joined by a middle dot.
#[test]
fn test_compaction_label_shape() {
    let row = compaction_row_text("754 turns, 5885 events", Some(ToggleHint::Expand));
    assert_eq!(
        row,
        "\u{273b} Context compacted (754 turns, 5885 events \u{00b7} ctrl+o to expand)"
    );
    let bare = compaction_row_text("", Some(ToggleHint::Expand));
    assert_eq!(bare, "\u{273b} Context compacted (ctrl+o to expand)");
    let unfolded = compaction_row_text("754 turns", None);
    assert_eq!(unfolded, "\u{273b} Context compacted (754 turns)");
}
