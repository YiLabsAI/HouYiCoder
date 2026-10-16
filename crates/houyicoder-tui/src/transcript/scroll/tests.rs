//! Transcript scrolling and search viewport tests.

use super::*;

use crate::row_id::RowId;

fn line(s: &str) -> TranscriptLine {
    TranscriptLine::System(s.to_string())
}

/// Park the scroll on a named row, the way a step does once it has named the
/// row it landed on.
fn parked(top: usize) -> TranscriptScroll {
    let mut s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(20);
    s.park(ScrollAnchor {
        row: RowId::local(7),
        char_offset: 0,
    });
    s.set_resolved_top(top);
    s
}

#[test]
fn test_follow_tail_tracks_growth() {
    let s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(20);
    assert!(s.is_following_tail());
    assert_eq!(s.top_offset(), 15);
    s.total.set(30);
    assert_eq!(
        s.top_offset(),
        25,
        "rows arriving at the tail carry the viewport with them"
    );
}

#[test]
fn test_parked_holds_resolved_row() {
    let s = parked(12);
    assert!(!s.is_following_tail());
    assert_eq!(s.top_offset(), 12);
    s.total.set(40);
    assert_eq!(
        s.top_offset(),
        12,
        "rows arriving below a parked viewport leave it where it stands"
    );
}

#[test]
fn test_clamp_reads_published_total() {
    // The clamp reads the published cells, and those only: nothing else can
    // say how far back the viewport may stand, so a later publish is what
    // moves it.
    let s = parked(18);
    assert_eq!(s.top_offset(), 15, "clamps to the published total");
    s.total.set(40);
    assert_eq!(s.top_offset(), 18, "a later publish loosens the clamp");
    s.total.set(10);
    assert_eq!(
        s.top_offset(),
        5,
        "a smaller publish clamps back, never past total - cap"
    );
}

#[test]
fn test_reached_tail_follows_again() {
    let mut s = parked(12);
    s.set_resolved_top(0);
    assert_eq!(s.top_offset(), 0, "the top row of the parked list");
    s.follow_tail();
    assert!(s.is_following_tail());
    assert_eq!(s.top_offset(), 15, "the tail is where the growth ends");
    assert!(
        s.anchor().is_none(),
        "the row the viewport stood on is let go on the way down"
    );
}

#[test]
fn test_scroll_room_needs_overflow() {
    let s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(5);
    assert!(
        !s.has_scroll_room(),
        "a transcript one screen long has none"
    );
    s.total.set(6);
    assert!(s.has_scroll_room());
}

#[test]
fn test_more_above_counts_hidden() {
    let s = parked(10);
    assert_eq!(s.more_above(), 10);
}

#[test]
fn test_search_finds_case_insensitive() {
    let mut s = TranscriptSearch {
        query: "BUG".to_string(),
        ..Default::default()
    };
    let t = vec![line("the bug is fixed"), line("nothing here")];
    s.run(&t);
    assert_eq!(s.matches, vec![0]);
    assert_eq!(s.focused_line(), Some(0));
}

#[test]
fn test_search_next_wraps() {
    let mut s = TranscriptSearch {
        query: "x".to_string(),
        ..Default::default()
    };
    let t = vec![line("x one"), line("x two"), line("none")];
    s.run(&t);
    assert_eq!(s.matches, vec![0, 1]);
    s.next();
    assert_eq!(s.focused_line(), Some(1));
    s.next();
    assert_eq!(s.focused_line(), Some(0));
}

#[test]
fn test_empty_query_finds_nothing() {
    let mut s = TranscriptSearch {
        query: String::new(),
        ..Default::default()
    };
    let t = vec![line("anything")];
    s.run(&t);
    assert!(s.matches.is_empty());
}

/// The walk's matcher and the view's are one decision in two shapes, so over one
/// corpus they never disagree on what the preparation rules carry: case,
/// surrounding space, a slash-command echo, and an empty query.
#[test]
fn test_match_shapes_agree() {
    let t = vec![
        line("the BUG is fixed"),
        line("  needled  "),
        TranscriptLine::User("/search needled".into()),
    ];

    for query in ["bug", "  BUG  ", "needled", "/search", "", "   ", "absent"] {
        assert_eq!(
            holds_matching_line(&t, query),
            !matching_lines(&t, query).is_empty(),
            "the two shapes of the matcher answer the same question for {query:?}"
        );
    }
    assert_eq!(
        matching_lines(&t, "needled"),
        vec![1],
        "the invocation echo is not content, so only the mention matches"
    );
}

mod anchor;
mod scrolling;
