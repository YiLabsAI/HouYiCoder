//! PTY test for the /search view: /search enters the full-screen verbose
//! search view (Scroll + verbose), the SEARCH chrome renders, n walks to the
//! older match and its line lands on screen, and q exits back to the working
//! surface. Pins #68/#69 closed end-to-end through the real binary (the unit
//! layer pins the highlight color + jump math; this pins the real crossterm
//! loop + wire + repaint chain: the view actually opens, the match is
//! visible, and the keys route).
//!
//! Run via make test ui (builds the bin first) or
//! cargo test --test ui_search -- --ignored after cargo build --bin houyi.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, session_on_working_with_script};

/// The reply carries "needle" so /search needle has a match in the agent
/// line; the user prompt "find needle please" is a SECOND match (an older
/// one), so n walks from the newest (the reply) to the older (the prompt).
const NEEDLE_SCRIPT: &str = r#"[
  [{"type":"Text","text":"needle found in the reply"}]
]"#;

/// /search enters the verbose view, the chrome + the focused match render, n
/// walks to the older match (the user prompt) so its text lands on screen,
/// and q exits to the working surface.
#[test]
#[ignore]
fn test_search_view_highlights_walks() {
    let mut s = session_on_working_with_script(NEEDLE_SCRIPT);
    // The user prompt is the older match; the scripted reply is the newest.
    s.send_str("find needle please");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("needle found in the reply", RENDER_TIMEOUT),
        "the scripted reply should land"
    );
    // /search needle enters the full-screen verbose search view.
    s.send_str("/search needle");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("SEARCH", RENDER_TIMEOUT),
        "the SEARCH chrome should render after /search"
    );
    assert!(
        s.wait_for("needle", RENDER_TIMEOUT),
        "the query + a match should be visible in the view"
    );
    // n walks to the older match (the user prompt) -- its text lands on screen.
    s.send_key(&Key::Char('n'));
    assert!(
        s.wait_for("find needle please", RENDER_TIMEOUT),
        "n should walk to the older match (the prompt) on screen"
    );
    // q exits the view back to the working surface.
    s.send_key(&Key::Char('q'));
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "q should exit the search view back to the working surface"
    );
}

/// The in-view slash re-search bar: inside the search view, slash opens a
/// 1-row input bar seeded with the current query; editing it and Enter
/// commits a fresh search (snapshot, not per-keystroke). Pins the real
/// crossterm loop + the bar chrome + the commit path end-to-end. The
/// discriminating assertion: committing a query word with NO matches shows
/// the no-match chrome -- a no-op commit (query unchanged, matches still
/// present) leaves the match count visible and no-match absent.
#[test]
#[ignore]
fn test_view_re_search_bar() {
    let mut s = session_on_working_with_script(NEEDLE_SCRIPT);
    s.send_str("find needle please");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("needle found in the reply", RENDER_TIMEOUT),
        "the scripted reply should land"
    );
    s.send_str("/search needle");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("SEARCH", RENDER_TIMEOUT),
        "the SEARCH chrome should render after /search"
    );
    // slash opens the in-view re-search bar. Assert the bar's unique
    // trailing chrome (Esc=cancel) -- the leading "Enter=re-search" can be
    // split by the renderer's diff skipping unchanged cells, but the
    // line-end span is written contiguously.
    s.send_key(&Key::Char('/'));
    assert!(
        s.wait_for_plain("Esc=cancel", RENDER_TIMEOUT),
        "the re-search input bar should render its chrome"
    );
    // Clear the seed and type a query with no matches, then commit.
    for _ in 0..6 {
        s.send_key(&Key::Backspace);
    }
    for c in "zzz".chars() {
        s.send_key(&Key::Char(c));
    }
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("no match", RENDER_TIMEOUT),
        "committing a no-match query should show the no-match chrome"
    );
    // q exits the view back to the working surface.
    s.send_key(&Key::Char('q'));
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "q should exit the search view back to the working surface"
    );
}
