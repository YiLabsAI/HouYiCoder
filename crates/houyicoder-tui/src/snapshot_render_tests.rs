//! Snapshot-search render invariants: count==render under a frozen
//! snapshot that differs from the live transcript, + eviction on the live
//! vec must not recompute the snapshot's matches. Split from
//! render_invariant_tests so both stay under the size gate.

use crate::records::TranscriptLine;
use crate::test_support::render_text;
use crate::transcript::snapshot::{SnapshotLoad, TranscriptSnapshot, WindowLoad};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::sync::Arc;

fn working() -> crate::state::App {
    crate::test_support::working_app()
}

use crate::test_support::MockSnapshot;

/// count==render must hold when the search view renders a frozen snapshot
/// that differs from the live transcript. The accessor routes count + render
/// through one source (active_transcript); a count path still reading
/// self.transcript would desync by the snapshot/live delta. Asserts visible
/// output (the snapshot content renders, the live content does not), not
/// internal state. Mutation-verified: reverting each of the six read points
/// (fold_aware_rows display_slots + line index, working_transcript
/// display_slots + line index + focused-range line) to self.transcript /
/// app.transcript reddens this test one at a time.
#[test]
fn test_count_matches_under_snapshot() {
    let mut app = working();
    app.transcript = vec![
        TranscriptLine::User("live alpha".into()),
        TranscriptLine::User("live beta".into()),
    ];
    app.search_transcript = vec![
        TranscriptLine::User("snap one".into()),
        TranscriptLine::User("snap two".into()),
        TranscriptLine::User("snap three".into()),
        TranscriptLine::User("snap four".into()),
        TranscriptLine::User("snap five".into()),
    ];
    app.search.active = true;
    app.search.query = "snap".into();
    app.run_search();
    // Focus the LAST match so the focused-range line index reaches the
    // snapshot's tail; a count path still reading self.transcript (2 lines)
    // would index out of bounds here.
    app.search.focus = app.search.matches.len().saturating_sub(1);
    app.verbose = true;
    // Pre-render: transcript_display_rows falls back to fold_aware_rows
    // (the Cell is 0 before any render), so this exercises the count path
    // reading active_transcript (5 snapshot lines, not 2 live lines).
    let pre_count = app.transcript_display_rows();
    assert!(
        pre_count > 3,
        "pre-render count must reflect the 5-line snapshot, not the 2-line live vec (got {pre_count})"
    );
    for w in [40, 80] {
        let out = render_text(&app, w, 50);
        let count = app.transcript_display_rows();
        let rendered = app.transcript_scroll.total.get();
        assert_eq!(
            count, rendered,
            "snapshot desync at w={w}: count={count} rendered={rendered}\n{out}"
        );
        assert!(
            out.contains("snap five"),
            "snapshot content must render at w={w}:\n{out}"
        );
        assert!(
            !out.contains("live alpha"),
            "live transcript must not render under snapshot at w={w}:\n{out}"
        );
    }
}

/// Eviction on the live transcript during an open search must not recompute
/// the snapshot's matches. The snapshot is frozen (no index shift), so the
/// prior recompute (the index-shift fix) is dead under the snapshot; keeping
/// it would overwrite matches against the wrong vec. Asserts the matches
/// survive eviction + the live vec is bounded (the recompute's absence is
/// the fix).
#[test]
fn test_bound_scrollback_skips_recompute() {
    let mut app = working();
    app.search_transcript = vec![
        TranscriptLine::User("snap match".into()),
        TranscriptLine::User("snap other".into()),
    ];
    app.search.active = true;
    app.search.query = "match".into();
    app.run_search();
    let matches_before = app.search.matches.clone();
    // Push past the 4000-row cap on the LIVE transcript to trigger eviction
    // while the search view is open. bound_scrollback must NOT recompute
    // search.matches against the drained live vec.
    for i in 0..4005 {
        app.push_transcript_line(TranscriptLine::User(format!("live drain {i}")));
    }
    assert_eq!(
        app.search.matches, matches_before,
        "eviction must not recompute the snapshot's matches"
    );
    assert!(
        app.transcript.len() <= 4000,
        "eviction bounded the live vec (len={})",
        app.transcript.len()
    );
}

/// The snapshot seam loads the WHOLE log, so a match beyond the live vec's
/// 4000-row cap is found (the cap no longer blinds search). The mock returns
/// 5001 lines with a needle at index 5000; the live transcript is empty, so
/// only the snapshot path can surface the match.
#[test]
fn test_uncapped_finds_old_match() {
    let mut app = working();
    let mut lines: Vec<TranscriptLine> = (0..5000)
        .map(|i| TranscriptLine::Agent(format!("filler {i}")))
        .collect();
    lines.push(TranscriptLine::User("needle deep in history".into()));
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines,
        log_bytes: 1024,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("needle");
    assert_eq!(
        app.search.matches,
        vec![5000],
        "the snapshot must find the match beyond the 4000-row cap"
    );
    let out = render_text(&app, 80, 50);
    assert!(
        out.contains("needle deep in history"),
        "the deep match must render:\n{out}"
    );
    assert!(!app.search_truncated, "a small log must not degrade");
}

/// I5 guard: search runs on the rendered projection, so a synthesized
/// chip name (Update, built from an edit event -- never in the raw log
/// bytes) is findable. A byte-level prefilter would miss it (the I5 bug).
#[test]
fn test_search_finds_synthesized_name() {
    let mut app = working();
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: vec![
            TranscriptLine::Tool {
                name: "Update".into(),
                tool: "edit".into(),
                status: String::new(),
                invocation: "src/lib.rs".into(),
                outcome: crate::records::ToolOutcome::Success,
                call_id: "c1".into(),
                body: String::new(),
                is_diff: false,
            },
            TranscriptLine::Agent("unrelated body text".into()),
        ],
        log_bytes: 1024,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("Update");
    assert!(
        !app.search.matches.is_empty(),
        "search must find the synthesized Update chip name (I5: search on rendered projection)"
    );
}

/// Over the 16 MB threshold, the snapshot degrades: load returns empty +
/// truncated, and the status bar surfaces an honest "log N MB too large"
/// hint -- not a silent "no match". The windowed load is the deferred
/// successor; for now the degrade is honest.
/// Over the threshold, the search view opens in byte-window mode (the real
/// path -- not a degrade). The window holds the tail (newest events),
/// search runs on the window's rendered projection, the chrome shows a byte
/// percentage (not "too large"), and frozen_file_size pins the boundary so
/// later appends stay invisible (I6). Asserts visible output + the mode flag,
/// not internal scroll state.
#[test]
fn test_large_log_opens_mode() {
    let mut app = working();
    let total = 20 * 1024 * 1024u64;
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: total,
        truncated: false,
        skipped: 0,
        window_lines: vec![
            TranscriptLine::User("tail needle here".into()),
            TranscriptLine::Agent("tail agent text".into()),
        ],
        window_start: 19 * 1024 * 1024,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("needle");
    assert!(app.window_mode, "over-threshold opens byte-window mode");
    assert!(
        !app.search_truncated,
        "window mode is the real path, not a degrade"
    );
    assert_eq!(app.frozen_file_size, total, "frozen at enter (I6 boundary)");
    assert_eq!(app.window_anchor, 19 * 1024 * 1024);
    assert_eq!(app.window_end, total, "tail window reaches EOF");
    assert!(
        app.search.matches.len() == 1,
        "search found the tail needle in the window"
    );
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("tail needle here"),
        "the window content renders:\n{out}"
    );
    // Byte-% position, not "too large" (the degrade path is gone).
    assert!(out.contains('%'), "byte-percent position in chrome:\n{out}");
    assert!(
        !out.contains("too large"),
        "no degrade hint in window mode:\n{out}"
    );
}

/// Corrupt log lines the tolerant read skipped surface in the chrome ("N
/// lines skipped") so the user sees data was dropped, not a silent gap.
#[test]
fn test_skipped_lines_in_chrome() {
    let mut app = working();
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: vec![TranscriptLine::User("needle in the snapshot".into())],
        log_bytes: 1024,
        truncated: false,
        skipped: 2,
        window_lines: Vec::new(),
        window_start: 0,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("needle");
    assert_eq!(app.search_skipped, 2, "skip count carried from the load");
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("2 lines skipped"),
        "skip count in the chrome:\n{out}"
    );
}

/// Exit clears the snapshot (frees the memory). Asserts search_transcript is
/// empty after exit. Note: RSS is not a user-visible quantity; the vec being
/// empty is the only assertable proxy for "freed memory" (do not change this
/// to a screen assertion -- it would test nothing).
#[test]
fn test_exit_clears_snapshot() {
    let mut app = working();
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: vec![
            TranscriptLine::User("snap one".into()),
            TranscriptLine::User("snap two".into()),
        ],
        log_bytes: 1024,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("snap");
    assert!(
        !app.search_transcript.is_empty(),
        "snapshot loaded on enter"
    );
    app.exit_search_view();
    assert!(
        app.search_transcript.is_empty(),
        "exit frees the snapshot vec"
    );
    assert!(!app.search_truncated, "truncated flag cleared");
    assert_eq!(app.search_skipped, 0, "skipped count cleared");
}

/// The default window/index methods on TranscriptSnapshot return empty
/// when not overridden. Uses a bare mock that only overrides log_size + load
/// so every other method falls through to the trait's default body, covering
/// those defaults so the diff-cov gate passes.
#[test]
fn test_snapshot_defaults_return_empty() {
    /// Bare mock: only log_size + load; window/tail_window/window_before/
    /// index_chunk/byte_at/event_count all use the trait defaults.
    struct Bare {
        log_bytes: u64,
    }
    impl TranscriptSnapshot for Bare {
        fn log_size(&self) -> u64 {
            self.log_bytes
        }
        fn load(&self, _max_bytes: u64) -> SnapshotLoad {
            SnapshotLoad::default()
        }
    }
    let mock = Bare { log_bytes: 1024 };
    let w = mock.window(0, 1024);
    assert!(w.lines.is_empty(), "default window is empty");
    let tw = mock.tail_window(1024);
    assert!(tw.lines.is_empty(), "default tail_window is empty");
    let wb = mock.window_before(512, 1024);
    assert!(wb.lines.is_empty(), "default window_before is empty");
    let p = mock.index_chunk();
    assert_eq!(p.indexed_bytes, 0, "default progress is zero");
    assert!(!p.done, "default progress not done");
    assert!(mock.byte_at(0).is_none(), "default byte_at is None");
    assert!(mock.event_count().is_none(), "default event_count is None");
}

/// Flat count==render invariant for the byte-window view: flat_display_rows
/// (the count path, which walks active_transcript + line_display_rows +
/// spacer, no fold slots) must equal the row count draw_flat_transcript
/// publishes to window_scroll.total. This is the window view's own
/// count==render pair -- it does NOT share fold_aware_rows, so
/// verbose_count_matches cannot cover it. The corpus carries a fold group
/// (proves the slot layer is skipped: a completed group renders as individual
/// call+result rows, not a Summary), a Thinking line (0 rows, skipped), an
/// Interrupted line (the no-spacer-before-child rule), and an expanded
/// ThoughtFor (header + reasoning rows), so every per-line branch of
/// push_line_rows + the spacer rule land in the walk.
///
/// Mutation verification (per AGENTS.md): reverting any divergence between
/// flat_walk's row count and draw_flat_transcript's row emission -- e.g.
/// flat_walk counting a spacer draw_flat_transcript skips, flat_walk NOT
/// skipping Thinking while the draw path does, or flat_walk dropping the
/// Interrupted no-spacer guard -- reddens this test at every width. Asserts
/// the published total (visible-output-adjacent), not an
/// internal flag.
#[test]
fn test_flat_count_matches_render() {
    let mut app = working();
    let long_reason = (0..8)
        .map(|i| format!("reason step {i} wrapping at narrow width"))
        .collect::<Vec<_>>()
        .join("\n");
    let window = vec![
        TranscriptLine::User("go".into()),
        // Interrupted is a child row of the message above (no blank spacer
        // before it) -- exercises the no-spacer-before-Interrupted rule both
        // push_line_rows and flat_walk share, so a regression to that guard
        // reddens this test.
        TranscriptLine::Interrupted,
        TranscriptLine::Thinking {
            text: "hidden thinking".into(),
        },
        TranscriptLine::Tool {
            name: "bash".into(),
            tool: "bash".into(),
            status: crate::brief::tool_call_brief(
                "bash",
                &serde_json::json!({"command": "echo hi"}),
            ),
            invocation: "echo hi".into(),
            outcome: crate::records::ToolOutcome::Success,
            call_id: "c1".into(),
            body: String::new(),
            is_diff: false,
        },
        TranscriptLine::Tool {
            name: "result".into(),
            tool: "bash".into(),
            status: String::new(),
            invocation: String::new(),
            outcome: crate::records::ToolOutcome::Success,
            call_id: "c1".into(),
            body: "hi\nthere".into(),
            is_diff: false,
        },
        TranscriptLine::ThoughtFor {
            secs: 3,
            turn_id: "t1".into(),
            reasoning: Some(long_reason),
            tool_summary: None,
        },
        TranscriptLine::Agent("answer paragraph".into()),
    ];
    let total = 20 * 1024 * 1024u64;
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: total,
        truncated: false,
        skipped: 0,
        window_lines: window,
        window_start: 19 * 1024 * 1024,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("answer");
    assert!(app.window_mode, "test setup: window mode active");
    for w in [40u16, 80, 120] {
        let out = render_text(&app, w, 24);
        let count = app.flat_display_rows();
        let rendered = app.window_scroll.total.get();
        assert_eq!(
            count, rendered,
            "flat count==render at w={w}: count={count} rendered={rendered}\n{out}"
        );
        // The fold group renders as individual call+result rows (the slot
        // layer is skipped); a Summary header would read "Ran" -- assert it
        // is absent so a regression to the slot path reddens this.
        assert!(
            !out.contains("Ran"),
            "no fold Summary in flat window:\n{out}"
        );
        assert!(
            out.contains("answer paragraph"),
            "window content renders at w={w}:\n{out}"
        );
    }
}

/// Exit clears the window state (frees the window vec + resets scroll + the
/// frozen boundary). Asserts the visible-mode flag + the boundary field, not
/// RSS (the vec being empty is the assertable proxy, per the exit-clears
/// convention).
#[test]
fn test_exit_clears_window_state() {
    let mut app = working();
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![
            TranscriptLine::User("tail one".into()),
            TranscriptLine::User("tail two".into()),
        ],
        window_start: 19 * 1024 * 1024,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    assert!(app.window_mode, "window mode entered");
    assert_eq!(app.frozen_file_size, 20 * 1024 * 1024);
    app.exit_search_view();
    assert!(!app.window_mode, "exit clears window mode");
    assert!(
        app.search_transcript.is_empty(),
        "exit frees the window vec"
    );
    assert_eq!(app.frozen_file_size, 0, "frozen boundary cleared");
    assert_eq!(app.window_anchor, 0);
    assert_eq!(app.window_end, 0);
}

/// In byte-window mode, g/G/PgUp/PgDn scroll the window_scroll (not
/// TranscriptScroll). Drives the real key dispatch (handle_key routes through
/// handle_search_view) so the window branches in pager_keys land. Asserts the
/// scroll state the status bar + render read, not internal flags.
#[test]
fn test_window_keys_scroll_window() {
    let mut app = working();
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 19 * 1024 * 1024,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    assert!(app.window_mode);
    // Simulate a render that published a window taller than one viewport so
    // paging is meaningful (the draw path sets these Cells; tests reach in to
    // avoid driving a full render just to seed scroll metrics).
    app.window_scroll.total.set(100);
    app.window_scroll.cap.set(10);
    // enter_search_view jumps to the focused match -> follow_tail false.
    assert!(!app.window_scroll.follow_tail);
    // G follows the tail (the bottom of the window).
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
    );
    assert!(app.window_scroll.follow_tail, "G follows the window tail");
    // g jumps to the top.
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    assert!(!app.window_scroll.follow_tail, "g pins to the top");
    assert_eq!(app.window_scroll.top_offset(), 0, "g at the top");
    // PageUp from the top is a no-op (already at 0); PageDown moves down.
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(
        app.window_scroll.top_offset(),
        0,
        "PageUp at top is a no-op"
    );
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
    );
    assert!(
        app.window_scroll.top_offset() > 0,
        "PageDown moves down in the window"
    );
}

/// Wheel scroll in byte-window mode drives window_scroll (not the live
/// transcript_scroll), and a left-button Down in the window does NOT start a
/// drag-selection (selection is deferred in window mode so the 5 scroll
/// consumers stay untouched). Drives the real mouse dispatch.
#[test]
fn test_window_scrolls_no_drag() {
    let mut app = working();
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 19 * 1024 * 1024,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    app.window_scroll.total.set(100);
    app.window_scroll.cap.set(10);
    let live_total_before = app.transcript_scroll.total.get();
    let rect = ratatui::layout::Rect::new(0, 0, 80, 24);
    app.transcript_rect.set(rect);
    // Wheel up: window_scroll pins (follow_tail -> false), transcript_scroll
    // untouched (the 5 consumers stay on their own path).
    crate::app::handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(!app.window_scroll.follow_tail, "wheel up pins the window");
    assert_eq!(
        app.transcript_scroll.total.get(),
        live_total_before,
        "wheel in window mode does not touch transcript_scroll"
    );
    // A left-button Down in the window does not start a transcript drag.
    crate::app::handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(
        !app.selection.is_dragging,
        "drag-select is deferred in window mode"
    );
}

/// n in byte-window mode walks toward the older match across windows. The
/// tail window holds one match at its newest position; pressing n (when it is
/// the only/oldest match in the window) loads the prior window, re-runs the
/// search, and focuses the closest older match. Asserts the loaded window +
/// byte anchor change to the older window, not internal scroll state.
#[test]
fn test_walks_older_across_windows() {
    let mut app = working();
    let total = 20 * 1024 * 1024u64;
    let windows = vec![
        WindowLoad {
            lines: vec![TranscriptLine::User("old needle".into())],
            start_offset: 0,
            next_offset: 100,
            skipped: 0,
            bytes_total: total,
        },
        WindowLoad {
            lines: vec![TranscriptLine::Agent("mid filler no match".into())],
            start_offset: 100,
            next_offset: 200,
            skipped: 0,
            bytes_total: total,
        },
        WindowLoad {
            lines: vec![TranscriptLine::User("new needle".into())],
            start_offset: 200,
            next_offset: total,
            skipped: 0,
            bytes_total: total,
        },
    ];
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: total,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows,
        index_steps: 0,
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("needle");
    assert_eq!(app.window_anchor, 200, "opens at the tail window");
    // n: tail window's match is the only one -> load the prior window. The
    // middle window has no match -> scan continues to the oldest window.
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    assert_eq!(app.window_anchor, 0, "n loaded the oldest window");
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("old needle"),
        "the older match renders after n:\n{out}"
    );
    assert!(
        !out.contains("new needle"),
        "the tail window's match is gone after n walked older:\n{out}"
    );
}

/// N in byte-window mode walks toward the newer match across windows. From
/// the oldest window, N loads newer windows until the newest match.
#[test]
fn test_walks_newer_across_windows() {
    let mut app = working();
    let total = 20 * 1024 * 1024u64;
    let windows = vec![
        WindowLoad {
            lines: vec![TranscriptLine::User("old needle".into())],
            start_offset: 0,
            next_offset: 100,
            skipped: 0,
            bytes_total: total,
        },
        WindowLoad {
            lines: vec![TranscriptLine::Agent("mid filler no match".into())],
            start_offset: 100,
            next_offset: 200,
            skipped: 0,
            bytes_total: total,
        },
        WindowLoad {
            lines: vec![TranscriptLine::User("new needle".into())],
            start_offset: 200,
            next_offset: total,
            skipped: 0,
            bytes_total: total,
        },
    ];
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: total,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows,
        index_steps: 0,
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("needle");
    // Walk to the oldest first (n), then N back to the newest.
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    assert_eq!(app.window_anchor, 0, "n reached the oldest window");
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE),
    );
    assert_eq!(app.window_anchor, 200, "N walked back to the tail window");
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("new needle"),
        "the newer match renders after N:\n{out}"
    );
}

/// G in byte-window mode starts the full-index build (indexing on); the
/// render path pumps index_chunk per frame. With a mock that completes in one
/// chunk, one pump flips indexing off + sets done. Asserts the mode flag +
/// the done cell, not internal index state.
#[test]
fn test_g_completes_full_index() {
    let mut app = working();
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 0,
        windows: Vec::new(),
        index_steps: 1, // completes in one chunk
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    assert!(!app.indexing.get(), "indexing off before G");
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
    );
    assert!(app.indexing.get(), "G starts the index build");
    // One render pump completes the build (the mock's index_steps == 1).
    app.pump_index_chunk();
    assert!(!app.indexing.get(), "index build completed + stopped");
    assert!(app.index_done.get(), "done flag set");
    let out = render_text(&app, 100, 24);
    // No longer indexing -> the chrome shows byte-%, not "indexing".
    assert!(
        !out.contains("indexing"),
        "chrome left the indexing state after completion:\n{out}"
    );
}

/// Esc while the full-index is building interrupts it (indexing off, view
/// stays open). Asserts the Esc is consumed by the interrupt (not exit) +
/// the view remains active.
#[test]
fn test_esc_interrupts_full_index() {
    let mut app = working();
    app.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 0,
        windows: Vec::new(),
        index_steps: 99, // never completes in the test -> Esc must interrupt
        index_calls: std::sync::atomic::AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    crate::app::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
    );
    assert!(app.indexing.get(), "G started the build");
    crate::app::handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.indexing.get(), "Esc interrupted the index build");
    assert!(
        app.search.active,
        "Esc during indexing stays in the view (does not exit)"
    );
}
