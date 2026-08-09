//! Interaction buffer-dump tests for transcript scrollback (PgUp/PgDn, End,
//! follow-tail) and search (inline Ctrl+F bar plus /search results popup). Each test renders the App to a
//! TestBackend and asserts on the real rendered text.

#![cfg(test)]

use crate::composition;
use crate::state::Screen;
use crate::test_support::{render_buffer, render_text};

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app
}

fn render(app: &crate::state::App) -> String {
    render_text(app, 100, 28)
}

/// Shared /search fixture: a 12-line tool-result body with the query on line 9
/// (mid-body), 60 filler rows after it, and a second match at the tail. This
/// is the search view's actual working condition -- multi-line expanded
/// content with the hit inside the range (not at the line head) and far from
/// the tail. Every search-view test should build on this so it does not
/// regress to single-line short-body minimal cases that hide the multi-row
/// highlight + jump behavior.
fn searchable_transcript() -> Vec<crate::records::TranscriptLine> {
    use crate::records::{ToolOutcome, TranscriptLine};
    let mut body = String::new();
    for i in 0..8 {
        body.push_str(&format!("body line {i}\n"));
    }
    body.push_str("needle mid-body line\n");
    body.push_str("body line 10\n");
    let mut t = vec![TranscriptLine::Tool {
        name: "result".into(),
        tool: "bash".into(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::Success,
        call_id: "c1".into(),
        body,
        is_diff: false,
    }];
    for i in 0..60 {
        t.push(TranscriptLine::Agent(format!("filler {i}")));
    }
    t.push(TranscriptLine::Agent("needle again newer".into()));
    t
}

#[test]
fn test_page_up_breaks_follow() {
    let mut app = working();
    for _ in 0..40 {
        app.system_line("a long line of transcript history");
    }
    let out = render(&app);
    println!("--- transcript at tail ---\n{out}\n--- end ---");
    assert!(app.transcript_scroll.follow_tail);
    // Render once so the view publishes cap/total into the Cell fields.
    drop(render(&app));
    app.scroll_transcript_up();
    assert!(
        !app.transcript_scroll.follow_tail,
        "PgUp should break follow-tail"
    );
    let out = render(&app);
    println!("--- transcript after PgUp ---\n{out}\n--- end ---");
    assert!(
        out.contains("+"),
        "N-more indicator should appear when scrolled back"
    );
    app.scroll_transcript_follow_tail();
    assert!(app.transcript_scroll.follow_tail);
}

/// PgUp then PgDown exercises the down path (which reads the published
/// total via transcript_display_rows, the unified source).
#[test]
fn test_page_down_after_up() {
    let mut app = working();
    for _ in 0..40 {
        app.system_line("a long line of transcript history");
    }
    drop(render(&app));
    app.scroll_transcript_up();
    assert!(!app.transcript_scroll.follow_tail);
    let top_after_up = app
        .transcript_scroll
        .top_offset(app.transcript_display_rows());
    app.scroll_transcript_down();
    let top_after_down = app
        .transcript_scroll
        .top_offset(app.transcript_display_rows());
    assert!(
        top_after_down >= top_after_up,
        "PgDown must not scroll further up: {top_after_up} -> {top_after_down}"
    );
}

/// The input caret (invert) hides when the terminal loses focus and re-shows
/// on refocus. The native cursor stays hidden + parked at the caret
/// (set_cursor_position) so IME preedit still lands correctly.
#[test]
fn test_focus_lost_hides_caret() {
    use crossterm::event::Event;
    use ratatui::style::Color;
    let mut app = working();
    app.input.set("abc".to_string());
    let has_white = |buf: &ratatui::buffer::Buffer| {
        (0..buf.area().height).any(|y| {
            (0..buf.area().width)
                .any(|x| buf.cell((x, y)).unwrap().style().bg == Some(Color::White))
        })
    };
    app.terminal_focused = true;
    let buf = render_buffer(&app, 80, 24);
    assert!(
        has_white(&buf),
        "caret should be inverted (bg White) when focused"
    );
    // FocusLost event hides the caret.
    assert!(crate::app::handle_event(&mut app, Event::FocusLost).unwrap());
    assert!(!app.terminal_focused, "FocusLost clears terminal_focused");
    let buf = render_buffer(&app, 80, 24);
    assert!(!has_white(&buf), "no inverted caret when focus lost");
    // FocusGained re-inverts.
    assert!(crate::app::handle_event(&mut app, Event::FocusGained).unwrap());
    assert!(app.terminal_focused, "FocusGained sets terminal_focused");
    let buf = render_buffer(&app, 80, 24);
    assert!(has_white(&buf), "caret re-inverted on refocus");
}

/// handle_event dispatches every terminal event variant without panic and
/// reports dirty. Covers the extracted match arms so the focus-gate
/// extraction does not regress coverage of the moved Key/Mouse/Paste/Resize
/// arms.
#[test]
fn test_handle_event_dispatches_each() {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
    let mk = |ev: Event| crate::app::handle_event(&mut working(), ev).unwrap();
    assert!(mk(Event::Key(KeyEvent::new(
        KeyCode::Char('a'),
        KeyModifiers::NONE
    ))));
    assert!(mk(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Moved,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE
    })));
    assert!(mk(Event::Paste("x".into())));
    assert!(mk(Event::Resize(80, 24)));
    assert!(mk(Event::FocusGained));
    assert!(mk(Event::FocusLost));
}

#[test]
fn test_agent_output_no_yank() {
    let mut app = working();
    for _ in 0..40 {
        app.system_line("history line");
    }
    drop(render(&app));
    app.scroll_transcript_up();
    assert!(!app.transcript_scroll.follow_tail);
    app.system_line("fresh line");
    // Agent output (a system line landing at turn end) must NOT yank a user
    // who scrolled back to read history. The scroll state is untouched by a
    // push; re-follow happens only at user action sites (submit, End, etc.).
    assert!(
        !app.transcript_scroll.follow_tail,
        "agent output yanked a scrolled-up user"
    );
}

fn context_app() -> crate::state::App {
    use houyicoder_protocol::frontend::SlashCommand;
    let mut app = working();
    app.push_transcript_line(crate::records::TranscriptLine::User("/context".into()));
    app.run_command(SlashCommand::Context);
    app
}

fn fill_transcript(app: &mut crate::state::App, n: usize) {
    app.push_transcript_line(crate::records::TranscriptLine::User("hi".into()));
    app.push_transcript_line(crate::records::TranscriptLine::Agent("hello back".into()));
    for _ in 0..n {
        app.push_transcript_line(crate::records::TranscriptLine::System("filler".into()));
    }
}

#[test]
fn test_grid_survives_page_up() {
    let mut app = context_app();
    fill_transcript(&mut app, 20);
    app.viewport = crate::state::ViewportMode::Scroll;
    drop(render(&app));
    let total = app.transcript_display_rows();
    app.transcript_scroll.page_up(total);
    let total = app.transcript_display_rows();
    app.transcript_scroll.page_up(total);
    let out = render(&app);
    assert!(out.contains("Context Usage"), "header gone: {out}");
    assert!(out.contains("Estimated usage"), "legend gone: {out}");
}

#[test]
fn test_context_grid_visible_top() {
    let mut app = context_app();
    fill_transcript(&mut app, 20);
    app.viewport = crate::state::ViewportMode::Scroll;
    app.transcript_scroll.follow_tail = false;
    app.transcript_scroll.offset = 0;
    drop(render(&app));
    let out = render(&app);
    assert!(
        out.contains("Context Usage"),
        "header missing at top: {out}"
    );
    let ue = out.lines().position(|l| l.contains("/context"));
    let gh = out.lines().position(|l| l.contains("Context Usage"));
    assert!(ue.is_some(), "user echo missing: {out}");
    assert!(gh.is_some(), "grid header missing: {out}");
    assert!(ue < gh, "user echo must be above grid header");
}

#[test]
fn test_context_grid_renders_overlap() {
    let mut app = context_app();
    fill_transcript(&mut app, 20);
    app.viewport = crate::state::ViewportMode::Scroll;
    drop(render(&app));
    app.transcript_scroll.follow_tail = false;
    app.transcript_scroll.offset = 5;
    let out = render(&app);
    assert!(
        out.contains("Estimated usage") || out.contains("Suggestions"),
        "block tail missing on partial overlap: {out}"
    );
}

#[test]
fn test_context_grid_pgup_working() {
    let mut app = context_app();
    fill_transcript(&mut app, 20);
    app.viewport = crate::state::ViewportMode::Working;
    drop(render(&app));
    app.enter_scroll();
    drop(render(&app));
    app.scroll_transcript_up();
    let out = render(&app);
    assert!(
        out.contains("Suggestions")
            || out.contains("Context Usage")
            || out.contains("Estimated usage")
            || out.contains("tokens"),
        "block not visible after PgUp from working: {out}"
    );
}

#[test]
fn test_slash_prefix_needs_boundary() {
    use crate::state::TranscriptLine;
    let mut app = working();
    app.input.set("/searchalot".to_string());
    app.submit_input();
    assert!(
        !app.search.active,
        "/searchalot must not open the search pane"
    );
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "/searchalot")),
    );

    let mut app = working();
    app.input.set("/rewindfoo".to_string());
    app.submit_input();
    assert!(
        !app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("unknown stage")),)
    );
}

// /search <query> enters the full-screen verbose search view, scrolled to the
// NEWEST match, and exit clears verbose + in_search + the query + restores
// the viewport. The render assertion pins the user-visible result -- the
// match LINE TEXT is on screen on entry -- not just the internal focus
// variable. Sixty filler lines after the match push it off the tail so the
// assertion discriminates: follow_tail alone leaves the match off-screen and
// reddens; jump_to_focused_match lands on it. The match line text (not the
// query word) is asserted because the query appears in the chrome.
#[test]
fn test_search_opens_newest_match() {
    use crate::records::TranscriptLine;
    use crate::state::ViewportMode;
    let mut app = working();
    let mut transcript = vec![TranscriptLine::Agent("needle in history".into())];
    for i in 0..60 {
        transcript.push(TranscriptLine::Agent(format!("filler line {i}")));
    }
    app.transcript = transcript;
    app.enter_search_view("needle");
    assert!(app.search.active, "in_search set on entry");
    assert!(app.verbose, "verbose set on entry");
    assert_eq!(app.viewport, ViewportMode::Scroll, "enters Scroll mode");
    let matches = &app.search.matches;
    assert_eq!(matches.len(), 1, "one needle match");
    assert_eq!(*matches.last().unwrap(), 0, "match is the first line");
    assert_eq!(matches[app.search.focus], 0, "focus = newest (only) match");
    // The match line must be ON SCREEN on entry, not just pointed at. The
    // filler pushes it 60 rows above the tail, so a follow_tail entry leaves
    // it off-screen and this reddens.
    let out = render(&app);
    assert!(
        out.contains("needle in history"),
        "focused match line must be ON SCREEN on entry, not just pointed at:\n{out}"
    );
    app.exit_search_view();
    assert!(!app.search.active, "in_search cleared on exit");
    assert!(!app.verbose, "verbose cleared on exit");
    assert_ne!(
        app.viewport,
        ViewportMode::Scroll,
        "viewport restored on exit"
    );
    assert!(app.search.query.is_empty(), "query cleared on exit");
}

// The search view renders a SEARCH chrome (the query + match count) and Esc
// exits it, clearing verbose + in_search. Pins the draw_scroll_status SEARCH
// branch + the handle_scroll in_search exit arm.
#[test]
fn test_search_chrome_exit_key() {
    use crate::records::TranscriptLine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.transcript = vec![TranscriptLine::Agent("foo here".into())];
    app.enter_search_view("foo");
    let out = render(&app);
    assert!(out.contains("SEARCH"), "chrome shows SEARCH: {out}");
    assert!(out.contains("foo"), "query in chrome: {out}");
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.search.active, "Esc exits the search view");
    assert!(!app.verbose, "Esc clears verbose");
}

// n walks toward the older matches, N toward the newer, and each jump lands
// the focused match on screen. Asserts RENDERED text (the walked-to match
// line), not the focus index -- a focus-only assertion would pass even if the
// screen never scrolled to the match.
#[test]
fn test_n_walks_toward_older() {
    use crate::records::TranscriptLine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    let mut transcript = vec![TranscriptLine::Agent("needle older mention".into())];
    for i in 0..60 {
        transcript.push(TranscriptLine::Agent(format!("filler {i}")));
    }
    transcript.push(TranscriptLine::Agent("needle newer mention".into()));
    app.transcript = transcript;
    app.enter_search_view("needle");
    let out = render(&app);
    assert!(
        out.contains("needle newer mention"),
        "entry at newest: {out}"
    );
    // n -> older match; its line text must be ON SCREEN after the jump.
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    let out = render(&app);
    assert!(
        out.contains("needle older mention"),
        "n walks to the older match on screen: {out}"
    );
    // N -> back to the newer match.
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE),
    );
    let out = render(&app);
    assert!(
        out.contains("needle newer mention"),
        "N walks back to the newer match on screen: {out}"
    );
}

// The focused (current) match carries a yellow background, distinct from the
// Cyan other matches. Asserted at the cell level (the visible highlight), not
// on an internal flag -- a flag-only assertion would pass even if the style
// never reached the screen.
// The focused match carries a yellow background on its query row, even when
// the query sits MID-BODY of a multi-line tool result (the verbose view's
// defining case). Built on searchable_transcript: enter focuses the trailing
// Agent match, then n walks to the older tool result whose query is on body
// line 9. A single-point current_row model marks only the body head and draws
// no yellow on the query row; the range model marks the whole body. Asserted
// at the cell level (visible), mutation-verified by reverting to the single
// point.
#[test]
fn test_search_current_match_yellow() {
    use crate::test_support::render_buffer;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Color;
    let mut app = working();
    app.transcript = searchable_transcript();
    app.enter_search_view("needle");
    // Warm the width (jump uses last_transcript_width set by a prior draw)
    // then walk to the older, multi-line tool-result match.
    drop(render(&app));
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    let buf = render_buffer(&app, 80, 24);
    let has_yellow = (0..buf.area().height).any(|y| {
        (0..buf.area().width).any(|x| {
            buf.cell((x, y))
                .map(|c| c.bg == Color::Yellow)
                .unwrap_or(false)
        })
    });
    assert!(
        has_yellow,
        "focused multi-line match must draw yellow on its mid-body query row"
    );
}

// The deleted inline/popup/disk surfaces stay deleted. The build catches
// references to deleted types; this guard catches a re-introduced DEAD
// definition (a struct/fn re-added but unused would compile). Walks source
// under crates (not docs -- bug-log and design docs legitimately carry these
// as history, and scanning docs would push someone to rewrite history).
// Excludes this test file so the pats literals do not self-match.
#[test]
fn test_legacy_search_definitions_gone() {
    use std::fs;
    use std::path::{Path, PathBuf};
    let pats = [
        "struct DiskHit",
        "trait SearchLog",
        "struct NoDiskLog",
        "struct SessionLogSearch",
        "fn run_popup_search",
        "fn open_inline_search",
        "fn close_search",
        "pub disk_matches",
        "pub disk_focus",
        "pub search_log",
    ];
    fn walk(dir: PathBuf, pats: &[&str], hits: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(p, pats, hits);
                continue;
            }
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".rs") || name == "scroll_tests.rs" {
                continue;
            }
            let Ok(src) = fs::read_to_string(&p) else {
                continue;
            };
            for pat in pats {
                if src.contains(pat) {
                    hits.push(format!("{}: {pat}", p.display()));
                }
            }
        }
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut hits = Vec::new();
    walk(root.join("src"), &pats, &mut hits);
    walk(root.join("../houyicoder-cli/src"), &pats, &mut hits);
    walk(root.join("../houyicoder-protocol/src"), &pats, &mut hits);
    assert!(
        hits.is_empty(),
        "legacy search definitions still in source:\n{}",
        hits.join("\n")
    );
}

// /search --all is recognized but full-history search is not wired into the
// new view: the user is TOLD (not silently degraded -- asking for MORE and
// getting less silently is worse than an error), then the in-memory window
// is searched.
#[test]
fn test_search_all_informs_user() {
    use crate::records::TranscriptLine;
    let mut app = working();
    app.transcript = vec![TranscriptLine::Agent("needle in window".into())];
    app.run_tui_local_command("search --all needle");
    assert!(app.search.active, "--all still enters the search view");
    assert!(app.verbose);
    assert!(
        app.transcript.iter().any(|l| matches!(
            l,
            TranscriptLine::System(s) if s.contains("full-history search is not in the new view yet")
        )),
        "must inform the user full-history is unavailable"
    );
    assert_eq!(
        app.search.matches.len(),
        1,
        "still searches the in-memory window"
    );
}

// The in-view slash re-search bar: pressing slash inside the search view
// opens a 1-row input bar seeded with the current query (less-style:
// slash shows the last pattern, editable). Asserted at the render level:
// the bar's chrome (Enter=re-search hint + the seeded query text) is on
// screen, not just the internal input_mode flag. A flag-only assertion
// would pass even if the bar never rendered.
#[test]
fn test_search_input_opens_seeded() {
    use crate::records::TranscriptLine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.transcript = vec![TranscriptLine::Agent("needle match".into())];
    app.enter_search_view("needle");
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
    );
    let out = render(&app);
    assert!(
        out.contains("Enter=re-search"),
        "input bar chrome visible: {out}"
    );
    assert!(
        out.contains("needle"),
        "bar seeded with the prior query: {out}"
    );
}

// Committing the in-view bar switches the query and jumps to the NEW
// query's newest match. The discriminating visible assertion is NEGATIVE:
// after commit the OLD query word is gone from the screen entirely (both
// the chrome and the old match line) -- a no-op commit (query unchanged,
// viewport still on the old match) leaves the old word on screen and
// reddens this. Built on a transcript with two distinct queryable terms so
// the switch is observable.
#[test]
fn test_search_commit_switches_query() {
    use crate::records::TranscriptLine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    let mut transcript = vec![TranscriptLine::Agent("needle older".into())];
    for i in 0..60 {
        transcript.push(TranscriptLine::Agent(format!("filler {i}")));
    }
    transcript.push(TranscriptLine::Agent("other newer".into()));
    app.transcript = transcript;
    app.enter_search_view("needle");
    assert!(render(&app).contains("needle older"));

    // Open the bar, clear the seed, type a new query, commit.
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
    );
    for _ in 0..6 {
        crate::keys::handle_working(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
    }
    for c in "other".chars() {
        crate::keys::handle_working(
            &mut app,
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
        );
    }
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let out = render(&app);
    assert!(
        out.contains("other newer"),
        "new query's newest match on screen: {out}"
    );
    assert!(
        out.contains("n=older"),
        "SEARCH chrome back (bar closed on commit): {out}"
    );
    assert!(
        !out.contains("needle"),
        "old query fully gone (chrome + old match): {out}"
    );
}

// Cancelling the in-view bar (Esc) restores the prior query and matches
// untouched -- the bar never mutated query/matches/focus (snapshot until
// commit), so cancel is a no-op restore. The visible assertion: the SEARCH
// chrome (with n=older suffix, only rendered when the bar is closed) is
// back, and the prior query word is still on screen. If cancel failed to
// close the bar, the input-bar chrome (Enter=re-search) would still show
// and n=older would be absent.
#[test]
fn test_search_cancel_restores_query() {
    use crate::records::TranscriptLine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.transcript = vec![TranscriptLine::Agent("needle match".into())];
    app.enter_search_view("needle");
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
    );
    // Type junk into the bar -- must NOT affect the committed query.
    for c in "xyz".chars() {
        crate::keys::handle_working(
            &mut app,
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
        );
    }
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let out = render(&app);
    assert!(
        out.contains("n=older"),
        "SEARCH chrome restored after cancel (bar closed): {out}"
    );
    assert!(
        out.contains("needle"),
        "prior query still on screen after cancel: {out}"
    );
}

// The in-view bar's cursor-edit keys (Left/Right/Home/End/Delete) and the
// empty-buffer chrome. Each edit is asserted at the render level: the bar
// re-renders the buffer text the edit produced, so a routing miss (a key
// falling to the no-op arm) leaves the prior text and reddens. Ctrl+C
// cancels like Esc (readline convention) -- asserted by the SEARCH chrome
// returning.
#[test]
fn test_search_input_edits_buffer() {
    use crate::records::TranscriptLine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.transcript = vec![TranscriptLine::Agent("needle match".into())];
    app.enter_search_view("needle");
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
    );
    // Clear the seed to empty -- the empty-buffer hint renders.
    for _ in 0..6 {
        crate::keys::handle_working(
            &mut app,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
    }
    assert!(
        render(&app).contains("(empty"),
        "empty-buffer hint: {}",
        render(&app)
    );
    // Type "abc", then Left + insert X -> "abXc" (Left routes to move_left).
    for c in "abc".chars() {
        crate::keys::handle_working(
            &mut app,
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
        );
    }
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE),
    );
    assert!(
        render(&app).contains("abXc"),
        "Left+insert: {}",
        render(&app)
    );
    // Home + Delete -> drops the first char -> "bXc" (Home + Delete route).
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
    assert!(
        render(&app).contains("bXc"),
        "Home+Delete: {}",
        render(&app)
    );
    // End pins the cursor at the tail; Right from End is a no-op (covers
    // End + Right routing); the buffer text is unchanged.
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert!(
        render(&app).contains("bXc"),
        "End+Right no-op: {}",
        render(&app)
    );
    // Ctrl+C cancels like Esc -- the SEARCH chrome returns.
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    );
    assert!(
        render(&app).contains("n=older"),
        "Ctrl+C cancels the bar: {}",
        render(&app)
    );
}

#[cfg(test)]
#[path = "hooks_tests.rs"]
mod hooks_tests;
