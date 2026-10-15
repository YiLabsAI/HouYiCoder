//! Transcript scrolling and search viewport tests.

use super::*;

fn line(s: &str) -> TranscriptLine {
    TranscriptLine::System(s.to_string())
}

#[test]
fn test_page_up_breaks_follow() {
    let mut s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(20);
    assert!(s.follow_tail);
    assert_eq!(s.top_offset(), 15);
    s.page_up();
    assert!(!s.follow_tail);
    assert_eq!(s.top_offset(), 10);
}

/// The clamp reads the cell, and the cell only. Before this contract the
/// step methods took total as an argument while the cell held the same
/// fact, so two sources could disagree about one value; this test pins
/// that a published cell change is what moves the clamp, with no second
/// source to override it.
#[test]
fn test_clamp_reads_published_total() {
    let mut s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(20);
    s.jump_to(18);
    assert_eq!(s.top_offset(), 15, "clamps to the published total");
    s.total.set(40);
    assert_eq!(s.top_offset(), 18, "a later publish moves the clamp");
    s.total.set(10);
    assert_eq!(
        s.top_offset(),
        5,
        "a smaller publish clamps back, never past total - cap"
    );
}

#[test]
fn test_page_down_reaches_tail() {
    let mut s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(20);
    s.page_up();
    assert!(!s.follow_tail);
    // page down once -> offset 15, still not at max_top(15) edge
    s.page_down();
    // one more page down reaches the bottom -> follow tail
    s.page_down();
    assert!(s.follow_tail);
}

#[test]
fn test_jump_to_pins_offset() {
    let mut s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(20);
    s.jump_to(3);
    assert!(!s.follow_tail);
    assert_eq!(s.top_offset(), 3);
}

#[test]
fn test_line_down_three() {
    let mut s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(20);
    // Pin near the middle first so a line step is visible vs a page jump.
    s.jump_to(5);
    assert_eq!(s.top_offset(), 5);
    s.line_down(3);
    assert_eq!(s.top_offset(), 8, "line step is 3, not a full page of 5");
    assert!(!s.follow_tail);
}

#[test]
fn test_line_up_three() {
    let mut s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(20);
    s.jump_to(8);
    s.line_up(3);
    assert_eq!(s.top_offset(), 5);
    assert!(!s.follow_tail);
}

#[test]
fn test_line_down_tail() {
    let mut s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(20);
    s.jump_to(14);
    // max_top = 20 - 5 = 15; stepping 3 from 14 reaches the tail.
    s.line_down(3);
    assert!(s.follow_tail, "reaching the bottom returns to follow-tail");
}

#[test]
fn test_line_up_clamp() {
    let mut s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(20);
    s.jump_to(1);
    s.line_up(3);
    assert_eq!(s.top_offset(), 0, "never underflows past the top");
}

#[test]
fn test_more_above_counts_hidden() {
    let mut s = TranscriptScroll::default();
    s.cap.set(5);
    s.total.set(20);
    s.page_up();
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

mod scrolling;
