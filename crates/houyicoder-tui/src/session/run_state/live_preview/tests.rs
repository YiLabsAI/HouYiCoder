//! The bound a live preview carries: what it keeps from each end of a
//! stream, what it releases in between, and what it still counts once
//! characters have been released.

use crate::view::line_wrap::{DISPLAY_HEAD_CHARS, DISPLAY_TAIL_CHARS};

use super::{LivePreview, RELEASE_QUANTUM, RETAINED_CHARS, char_offset};

/// Stream a text into the preview in fixed-size chunks, the way a reply
/// arrives delta by delta rather than in one piece.
fn stream(preview: &mut LivePreview, text: &str, chunk: usize) {
    let mut rest = text;
    while !rest.is_empty() {
        let cut = char_offset(rest, chunk);
        let (delta, tail) = rest.split_at(cut);
        preview.push_str(delta);
        rest = tail;
    }
}

/// Three runs of distinct characters, so a released middle is recognisable
/// as absent and each end as present. No newlines, so the display form's
/// characters are exactly the stretches it still shows.
fn head_middle_tail() -> String {
    format!(
        "{}{}{}",
        "H".repeat(DISPLAY_HEAD_CHARS + 500),
        "M".repeat(10_000),
        "T".repeat(DISPLAY_TAIL_CHARS + 500)
    )
}

/// The released count the display form's marker names, and the characters it
/// still shows, as two independent readings of the same text.
fn marker_and_shown(shown: &str) -> (usize, usize) {
    let marker = shown
        .lines()
        .find(|line| line.starts_with('…'))
        .expect("a released middle carries a marker");
    let named: usize = marker
        .strip_prefix("… +")
        .and_then(|rest| rest.strip_suffix(" chars …"))
        .and_then(|digits| digits.parse().ok())
        .expect("the marker names a count");
    let shown_chars: usize = shown
        .lines()
        .filter(|line| !line.starts_with('…'))
        .map(|line| line.chars().count())
        .sum();
    (named, shown_chars)
}

#[test]
fn test_short_stream_keeps_all() {
    let mut preview = LivePreview::default();
    preview.push_str("hello ");
    preview.push_str("world");
    assert_eq!(preview.as_str(), "hello world");
    assert_eq!(preview.streamed_chars(), 11);
    assert!(!preview.is_empty());
}

#[test]
fn test_at_bound_nothing_released() {
    let mut preview = LivePreview::default();
    stream(&mut preview, &"a".repeat(RETAINED_CHARS), 1_000);
    assert_eq!(preview.as_str().chars().count(), RETAINED_CHARS);
    assert!(
        !preview.as_str().contains('…'),
        "nothing is released at the bound: {:?}",
        preview.as_str()
    );
}

#[test]
fn test_release_waits_for_quantum() {
    let mut preview = LivePreview::default();
    preview.push_str(&"a".repeat(RETAINED_CHARS + RELEASE_QUANTUM));
    assert!(
        !preview.as_str().contains('…'),
        "an excess within the quantum is not released: {} chars",
        preview.as_str().chars().count()
    );
    preview.push_str("a");
    assert!(
        preview
            .as_str()
            .contains(&format!("… +{} chars …", RELEASE_QUANTUM + 1)),
        "the release names the whole excess, the quantum and this character: {:?}",
        preview.as_str()
    );
}

#[test]
fn test_long_stream_keeps_ends() {
    let mut preview = LivePreview::default();
    stream(&mut preview, &head_middle_tail(), 1_000);
    let shown = preview.as_str();
    assert!(
        shown.starts_with(&"H".repeat(DISPLAY_HEAD_CHARS)),
        "the head of the stream stays: {:?}",
        &shown[..40]
    );
    assert!(
        shown.ends_with(&"T".repeat(DISPLAY_TAIL_CHARS)),
        "the newest end stays: {:?}",
        &shown[shown.len() - 40..]
    );
    assert!(
        !shown.contains('M'),
        "the released middle is gone from the display form"
    );
}

/// What arrived equals what the display form still shows plus what its marker
/// names, however the stream was chunked: the marker's count and the two
/// stretches on screen are one account.
#[test]
fn test_release_accounts_whole_stream() {
    let mut preview = LivePreview::default();
    stream(&mut preview, &head_middle_tail(), 1_000);
    let (named, shown_chars) = marker_and_shown(preview.as_str());
    assert_eq!(
        named + shown_chars,
        preview.streamed_chars(),
        "released {named} plus shown {shown_chars} must cover the stream: {:?}",
        preview.as_str()
    );
}

#[test]
fn test_long_stream_bounds_display() {
    let mut preview = LivePreview::default();
    stream(&mut preview, &"x".repeat(100_000), 1_000);
    let shown_chars = preview.as_str().chars().count();
    assert!(
        shown_chars >= RETAINED_CHARS,
        "both ends are kept, so the display form holds at least the bound: {shown_chars}"
    );
    assert!(
        shown_chars <= RETAINED_CHARS + RELEASE_QUANTUM + 64,
        "the display form stays at the retained bound plus one release quantum, not at the stream's size: {shown_chars}"
    );
}

#[test]
fn test_streamed_count_outgrows_display() {
    let mut preview = LivePreview::default();
    stream(&mut preview, &"y".repeat(100_000), 1_000);
    assert_eq!(
        preview.streamed_chars(),
        100_000,
        "the count covers the whole stream, so an estimate over it keeps climbing"
    );
    assert!(
        preview.streamed_chars() > preview.as_str().chars().count(),
        "the display form is a bounded view of the stream, not the stream"
    );
}

#[test]
fn test_multibyte_stream_stays_bounded() {
    let mut preview = LivePreview::default();
    stream(&mut preview, &"漢".repeat(20_000), 700);
    let shown = preview.as_str();
    assert!(
        shown.chars().count() <= RETAINED_CHARS + RELEASE_QUANTUM + 64,
        "a multi-byte stream is bounded by characters, not bytes"
    );
    assert!(shown.starts_with('漢') && shown.ends_with('漢'));
}

#[test]
fn test_clear_restarts_stream() {
    let mut preview = LivePreview::default();
    stream(&mut preview, &"z".repeat(20_000), 1_000);
    preview.clear();
    assert!(preview.is_empty());
    assert_eq!(preview.as_str(), "");
    assert_eq!(preview.streamed_chars(), 0);
    preview.push_str("new");
    assert_eq!(preview.as_str(), "new");
    assert_eq!(preview.streamed_chars(), 3);
}
