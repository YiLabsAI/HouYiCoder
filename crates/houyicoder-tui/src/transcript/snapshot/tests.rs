//! Snapshot-search render invariants: count==render under a frozen
//! snapshot that differs from the live transcript, + eviction on the live
//! vec must not recompute the snapshot's matches. Split from
//! render_invariant_tests so both stay under the size gate.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::agent_message::SessionMessage;
use crate::app::{handle_key, handle_mouse};
use crate::brief::tool_call_brief;
use crate::records::{ToolOutcome, TranscriptLine};
use crate::session::SessionConnection;
use crate::state::App;
use crate::state::index_read::PendingIndexChunk;
use crate::test_harness::{MockSnapshot, render_text, working_app};
use crate::transcript::snapshot::{
    IndexProgress, NoTranscriptSnapshot, TranscriptSnapshot, WindowLoad,
};

fn working() -> App {
    working_app()
}

/// A working App whose session holds a receiver-backed connection, so the
/// index build dispatches on a worker rather than reading inline.
fn connected_working() -> App {
    let mut app = working();
    let (_tx, rx) = channel::<SessionMessage>();
    app.sessions.current.connection = Some(SessionConnection::from_receiver(rx));
    app
}

/// count==render must hold when the search view renders a frozen snapshot
/// that differs from the live transcript. The accessor routes count + render
/// through one source (active_transcript); a count path still reading
/// self.sessions.current.transcript would desync by the snapshot/live delta. Asserts visible
/// output (the snapshot content renders, the live content does not), not
/// internal state. Mutation-verified: reverting each of the six read points
/// (fold_aware_rows display_slots + line index, transcript renderer
/// display_slots + line index + focused-range line) to self.sessions.current.transcript /
/// app.sessions.current.transcript reddens this test one at a time.
#[test]
fn test_count_matches_under_snapshot() {
    let mut app = working();
    app.sessions.current.transcript = vec![
        TranscriptLine::User("live alpha".into()),
        TranscriptLine::User("live beta".into()),
    ]
    .into();
    app.ui.transcript_view.replace_corpus_lines(vec![
        TranscriptLine::User("snap one".into()),
        TranscriptLine::User("snap two".into()),
        TranscriptLine::User("snap three".into()),
        TranscriptLine::User("snap four".into()),
        TranscriptLine::User("snap five".into()),
    ]);
    app.ui
        .transcript_view
        .set_search_state_for_test(true, "snap", vec![], 0);
    app.run_search();
    // Focus the LAST match so the focused-range line index reaches the
    // snapshot's tail; a count path still reading self.sessions.current.transcript (2 lines)
    // would index out of bounds here.
    app.ui.transcript_view.search_focus_last();
    app.ui.verbose = true;
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
        let rendered = app.whole_scroll().total.get();
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
    app.ui.transcript_view.replace_corpus_lines(vec![
        TranscriptLine::User("snap match".into()),
        TranscriptLine::User("snap other".into()),
    ]);
    app.ui
        .transcript_view
        .set_search_state_for_test(true, "match", vec![], 0);
    app.run_search();
    let matches_before = app.ui.transcript_view.search_matches().to_vec();
    // Push past the 4000-row cap on the LIVE transcript to trigger eviction
    // while the search view is open. bound_scrollback must NOT recompute
    // search.matches against the drained live vec.
    for i in 0..4005 {
        app.push_transcript_line(TranscriptLine::User(format!("live drain {i}")));
    }
    assert_eq!(
        app.ui.transcript_view.search_matches(),
        matches_before.as_slice(),
        "eviction must not recompute the snapshot's matches"
    );
    assert!(
        app.sessions.current.transcript.len() <= 4000,
        "eviction bounded the live vec (len={})",
        app.sessions.current.transcript.len()
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
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines,
        log_bytes: 1024,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("needle");
    assert_eq!(
        app.ui.transcript_view.search_matches(),
        &[5000],
        "the snapshot must find the match beyond the 4000-row cap"
    );
    let out = render_text(&app, 80, 50);
    assert!(
        out.contains("needle deep in history"),
        "the deep match must render:\n{out}"
    );
    assert!(
        !app.ui.transcript_view.search_corpus_truncated(),
        "a small log must not degrade"
    );
}

/// Search runs on the rendered rows, so a synthesized chip name (Update,
/// built from an edit event — never in the raw log bytes) is findable. A
/// byte-level prefilter would miss it.
#[test]
fn test_search_finds_synthesized_name() {
    let mut app = working();
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: vec![
            TranscriptLine::Tool {
                name: "Update".into(),
                tool: "edit".into(),
                status: String::new(),
                invocation: "src/lib.rs".into(),
                outcome: ToolOutcome::Success,
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
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("Update");
    assert!(
        !app.ui.transcript_view.search_matches().is_empty(),
        "search must find the synthesized Update chip name: search reads the rendered rows"
    );
}

/// Over the 16 MB threshold, the search view opens in byte-window mode. The
/// window holds the tail (newest events), search runs on the window's rendered
/// rows, the chrome shows a byte percentage (not "too large"), and
/// frozen_file_size pins the boundary so later appends stay invisible. Asserts
/// visible output + the mode flag, not internal scroll state.
#[test]
fn test_large_log_opens_mode() {
    let mut app = working();
    let total = 20 * 1024 * 1024u64;
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
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
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("needle");
    assert!(
        app.window_browsing(),
        "over-threshold opens byte-window mode"
    );
    assert!(
        !app.ui.transcript_view.search_corpus_truncated(),
        "window mode is the real path, not a degrade"
    );
    assert_eq!(
        app.frozen_file_size(),
        total,
        "frozen at enter, so later appends stay outside the window"
    );
    assert_eq!(app.window_anchor(), 19 * 1024 * 1024);
    assert_eq!(app.window_end(), total, "tail window reaches EOF");
    assert!(
        app.ui.transcript_view.search_matches_len() == 1,
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
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: vec![TranscriptLine::User("needle in the snapshot".into())],
        log_bytes: 1024,
        truncated: false,
        skipped: 2,
        window_lines: Vec::new(),
        window_start: 0,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("needle");
    assert_eq!(
        app.ui.transcript_view.search_corpus_skipped(),
        2,
        "skip count carried from the load"
    );
    let out = render_text(&app, 100, 24);
    assert!(
        out.contains("2 lines skipped"),
        "skip count in the chrome:\n{out}"
    );
}

/// Exit clears the snapshot (frees the memory). Asserts the corpus vec is
/// empty after exit. Note: RSS is not a user-visible quantity; the vec being
/// empty is the only assertable proxy for "freed memory" (do not change this
/// to a screen assertion -- it would test nothing).
#[test]
fn test_exit_clears_snapshot() {
    let mut app = working();
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
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
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("snap");
    assert!(
        !app.ui.transcript_view.search_corpus_lines().is_empty(),
        "snapshot loaded on enter"
    );
    app.exit_search_view();
    assert!(
        app.ui.transcript_view.search_corpus_lines().is_empty(),
        "exit frees the snapshot vec"
    );
    assert!(
        !app.ui.transcript_view.search_corpus_truncated(),
        "truncated flag cleared"
    );
    assert_eq!(
        app.ui.transcript_view.search_corpus_skipped(),
        0,
        "skipped count cleared"
    );
}

/// A source with no on-disk log answers every window empty and holds no index:
/// the type says so by name, which is what a reader needs, since log_size alone
/// can route an implementation into the windowed path.
#[test]
fn test_no_snapshot_answers_empty() {
    let source = NoTranscriptSnapshot;
    assert_eq!(source.log_size(), 0);
    assert!(source.load(1024).lines.is_empty(), "load is empty");
    assert!(source.window(0, 1024).lines.is_empty(), "window is empty");
    assert!(
        source.tail_window(1024).unwrap().lines.is_empty(),
        "tail_window is empty"
    );
    assert!(
        source.window_before(512, 1024).unwrap().lines.is_empty(),
        "window_before is empty"
    );
    let progress = source.index_chunk();
    assert_eq!(progress.indexed_bytes, 0, "nothing is indexed");
    assert!(!progress.done, "and the build is not claimed done");
    assert!(source.byte_at(0).is_none(), "no event offset is known");
    assert!(source.event_count().is_none(), "and no count either");
}

/// Flat count==render invariant for the byte-window view: history_display_rows
/// (the count path, which walks active_transcript + line_display_rows +
/// spacer, no fold slots) must equal the row count draw_history_window
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
/// history_walk's row count and draw_history_window's row emission -- e.g.
/// history_walk counting a spacer draw_history_window skips, history_walk NOT
/// skipping Thinking while the draw path does, or history_walk dropping the
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
        // push_line_rows and history_walk share, so a regression to that guard
        // reddens this test.
        TranscriptLine::Interrupted,
        TranscriptLine::Thinking {
            text: "hidden thinking".into(),
        },
        TranscriptLine::Tool {
            name: "bash".into(),
            tool: "bash".into(),
            status: tool_call_brief("bash", &serde_json::json!({"command": "echo hi"})),
            invocation: "echo hi".into(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            body: String::new(),
            is_diff: false,
        },
        TranscriptLine::Tool {
            name: "result".into(),
            tool: "bash".into(),
            status: String::new(),
            invocation: String::new(),
            outcome: ToolOutcome::Success,
            call_id: "c1".into(),
            body: "hi\nthere".into(),
            is_diff: false,
        },
        TranscriptLine::ThoughtFor {
            ms: Some(3_000),
            turn_id: "t1".into(),
            reasoning: Some(long_reason),
            tool_summary: None,
        },
        TranscriptLine::Agent("answer paragraph".into()),
    ];
    let total = 20 * 1024 * 1024u64;
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: total,
        truncated: false,
        skipped: 0,
        window_lines: window,
        window_start: 19 * 1024 * 1024,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("answer");
    assert!(app.window_browsing(), "test setup: window mode active");
    for w in [40u16, 80, 120] {
        let out = render_text(&app, w, 24);
        let count = app.history_display_rows();
        let rendered = app.window_scroll().total.get();
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
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
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
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    assert!(app.window_browsing(), "window mode entered");
    assert_eq!(app.frozen_file_size(), 20 * 1024 * 1024);
    app.exit_search_view();
    assert!(!app.window_browsing(), "exit clears window mode");
    assert!(
        app.ui.transcript_view.search_corpus_lines().is_empty(),
        "exit frees the window vec"
    );
    assert_eq!(app.frozen_file_size(), 0, "frozen boundary cleared");
    assert_eq!(app.window_anchor(), 0);
    assert_eq!(app.window_end(), 0);
}

/// In byte-window mode, g/G/PgUp/PgDn scroll the window_scroll (not
/// TranscriptScroll). Drives the real key dispatch (handle_key routes through
/// handle_search_view) so the window branches in pager_keys land. Asserts the
/// scroll state the status bar + render read, not internal flags.
#[test]
fn test_window_keys_scroll_window() {
    let mut app = working();
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 19 * 1024 * 1024,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    assert!(app.window_browsing());
    // Simulate a render that published a window taller than one viewport so
    // paging is meaningful (the draw path sets these Cells; tests reach in to
    // avoid driving a full render just to seed scroll metrics).
    app.window_scroll().total.set(100);
    app.window_scroll().cap.set(10);
    // enter_search_view jumps to the focused match -> follow_tail false.
    assert!(!app.window_scroll().is_following_tail());
    // G follows the tail (the bottom of the window).
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
    );
    assert!(
        app.window_scroll().is_following_tail(),
        "G follows the window tail"
    );
    // g jumps to the top.
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    assert!(
        !app.window_scroll().is_following_tail(),
        "g pins to the top"
    );
    assert_eq!(app.window_scroll().top_offset(), 0, "g at the top");
    // PageUp from the top is a no-op (already at 0); PageDown moves down.
    handle_key(&mut app, KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    assert_eq!(
        app.window_scroll().top_offset(),
        0,
        "PageUp at top is a no-op"
    );
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
    );
    assert!(
        app.window_scroll().top_offset() > 0,
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
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 19 * 1024 * 1024,
        windows: Vec::new(),
        index_steps: 0,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    app.window_scroll().total.set(100);
    app.window_scroll().cap.set(10);
    let live_total_before = app.whole_scroll().total.get();
    let rect = Rect::new(0, 0, 80, 24);
    app.ui.frame.layout.transcript_rect.set(rect);
    // Wheel up: window_scroll pins (follow_tail -> false), transcript_scroll
    // untouched (the 5 consumers stay on their own path).
    handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(
        !app.window_scroll().is_following_tail(),
        "wheel up pins the window"
    );
    assert_eq!(
        app.whole_scroll().total.get(),
        live_total_before,
        "wheel in window mode does not touch transcript_scroll"
    );
    // A left-button Down in the window does not start a transcript drag.
    handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(
        !app.ui.selection.transcript.is_dragging,
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
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: total,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows,
        index_steps: 0,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("needle");
    assert_eq!(app.window_anchor(), 200, "opens at the tail window");
    // n: tail window's match is the only one -> load the prior window. The
    // middle window has no match -> scan continues to the oldest window.
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    assert_eq!(app.window_anchor(), 0, "n loaded the oldest window");
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
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: total,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows,
        index_steps: 0,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("needle");
    // Walk to the oldest first (n), then N back to the newest.
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
    );
    assert_eq!(app.window_anchor(), 0, "n reached the oldest window");
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE),
    );
    assert_eq!(app.window_anchor(), 200, "N walked back to the tail window");
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
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 0,
        windows: Vec::new(),
        index_steps: 1, // completes in one chunk
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    assert!(
        !app.sessions.current.index_build.indexing.get(),
        "indexing off before G"
    );
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
    );
    assert!(
        app.sessions.current.index_build.indexing.get(),
        "G starts the index build"
    );
    // One render pump completes the build (the mock's index_steps == 1).
    app.pump_index_chunk();
    assert!(
        !app.sessions.current.index_build.indexing.get(),
        "index build completed + stopped"
    );
    assert!(
        app.sessions.current.index_build.index_done.get(),
        "done flag set"
    );
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
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 0,
        windows: Vec::new(),
        index_steps: 99, // never completes in the test -> Esc must interrupt
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
    );
    assert!(
        app.sessions.current.index_build.indexing.get(),
        "G started the build"
    );
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        !app.sessions.current.index_build.indexing.get(),
        "Esc interrupted the index build"
    );
    assert!(
        app.ui.transcript_view.search_active(),
        "Esc during indexing stays in the view (does not exit)"
    );
}

/// The shared mock drives the windowed path from prebuilt windows and models no
/// event-to-offset index, so it reports none: a caller that needs an event
/// offset is told the source cannot name one rather than being handed a wrong
/// byte.
#[test]
fn test_mock_has_no_index() {
    let mock = MockSnapshot {
        lines: Vec::new(),
        log_bytes: 4096,
        truncated: false,
        skipped: 0,
        window_lines: Vec::new(),
        window_start: 0,
        windows: Vec::new(),
        index_steps: 1,
        index_calls: AtomicU32::new(0),
    };
    assert!(mock.byte_at(0).is_none(), "the mock names no event offset",);
    assert!(mock.event_count().is_none(), "and no event count");
}

/// Drawing never reads the log for the index: a chunk is up to four megabytes
/// and parses every line in it, so the build advances on the loop instead. A
/// draw while the build is running leaves the chunk unread.
#[test]
fn test_draw_reads_no_index() {
    let mut app = working();
    let source = Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 0,
        windows: Vec::new(),
        index_steps: 3,
        index_calls: AtomicU32::new(0),
    });
    app.sessions.persistence.snapshot = Some(source.clone());
    app.enter_search_view("tail");
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
    );
    assert!(
        app.sessions.current.index_build.indexing.get(),
        "G starts the index build"
    );

    let calls_before = source.index_calls.load(Ordering::Relaxed);
    let drawn = render_text(&app, 100, 24);
    assert!(!drawn.is_empty(), "the frame drew");
    let calls_after = source.index_calls.load(Ordering::Relaxed);
    assert_eq!(
        calls_after, calls_before,
        "a draw reads no index chunk: {calls_before} then {calls_after}"
    );
}

/// With a live connection, a chunk runs on a worker: the loop advances the
/// build and applies what the worker sent.
#[test]
fn test_index_runs_on_worker() {
    let mut app = connected_working();
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 0,
        windows: Vec::new(),
        index_steps: 1,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
    );
    assert!(
        app.sessions.current.index_build.indexing.get(),
        "G starts the build"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if app.pump_background_reads() && app.sessions.current.index_build.index_done.get() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        app.sessions.current.index_build.index_done.get(),
        "the worker's chunk finished the build"
    );
    assert!(
        !app.sessions.current.index_build.indexing.get(),
        "and the build stopped"
    );
}

/// A worker that dies without sending stops the build rather than stalling it:
/// the record is cleared and indexing turns off, so a later G can start again.
#[test]
fn test_index_worker_death_stops() {
    let mut app = working();
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 0,
        windows: Vec::new(),
        index_steps: 1,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
    );
    // A record whose sender is already gone stands in for a dead worker.
    let (tx, rx) = channel();
    drop(tx);
    *app.sessions
        .current
        .index_build
        .pending_index_chunk
        .borrow_mut() = Some(PendingIndexChunk::new(rx));

    assert!(app.pump_index_chunk(), "the dead worker is noticed");
    assert!(
        !app.sessions.current.index_build.indexing.get(),
        "and the build stops rather than stalling"
    );
    assert!(
        app.sessions
            .current
            .index_build
            .pending_index_chunk
            .borrow()
            .is_none(),
        "the record is cleared so a later G can dispatch again"
    );
}

/// An interrupt stops the build but the chunk already on its worker still
/// lands. The partial index is kept by contract, so the landing applies its
/// progress without resuming the interrupted build; a later G restarts and
/// completes from there.
#[test]
fn test_index_landing_after_interrupt() {
    let mut app = working();
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 0,
        windows: Vec::new(),
        index_steps: 1,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
    );
    assert!(
        app.sessions.current.index_build.indexing.get(),
        "G starts the build"
    );
    // A held sender stands in for the chunk's worker.
    let (tx, rx) = channel();
    *app.sessions
        .current
        .index_build
        .pending_index_chunk
        .borrow_mut() = Some(PendingIndexChunk::new(rx));

    assert!(app.interrupt_index(), "the interrupt consumed the Esc");
    tx.send(IndexProgress {
        indexed_bytes: 512,
        total_bytes: 2048,
        done: false,
    })
    .ok();

    assert!(app.pump_index_chunk(), "the landed chunk is noticed");
    assert_eq!(
        app.sessions.current.index_build.indexed_bytes.get(),
        512,
        "its progress applied"
    );
    assert!(
        !app.sessions.current.index_build.indexing.get(),
        "the interrupted build did not resume on the landing"
    );

    // Restart with a connection: the build runs on a worker to completion.
    let (_tx, rx) = channel::<SessionMessage>();
    app.sessions.current.connection = Some(SessionConnection::from_receiver(rx));
    app.start_full_index();
    assert!(
        app.sessions.current.index_build.indexing.get(),
        "G restarts the build"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if app.pump_background_reads() && app.sessions.current.index_build.index_done.get() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        app.sessions.current.index_build.index_done.get(),
        "the restarted build completed"
    );
}

/// A build already running is left to the loop. The key that starts one carries
/// no gate of its own, so pressing it again reaches the dispatch, and a second
/// chunk would overwrite the record: the receiver of the running chunk is
/// dropped, which orphans a worker that reads the log and sends into a channel
/// nobody holds. The sender held here is the observation -- it keeps its
/// receiver only if no second chunk was dispatched.
#[test]
fn test_index_restart_waits() {
    let mut app = connected_working();
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 0,
        windows: Vec::new(),
        index_steps: 1,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    app.sessions.current.index_build.indexing.set(true);
    let (tx, rx) = channel();
    *app.sessions
        .current
        .index_build
        .pending_index_chunk
        .borrow_mut() = Some(PendingIndexChunk::new(rx));

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE),
    );

    assert!(
        tx.send(IndexProgress {
            indexed_bytes: 7,
            total_bytes: 9,
            done: false,
        })
        .is_ok(),
        "the running chunk still has its receiver"
    );
    assert!(app.pump_index_chunk(), "and its progress still lands");
    assert_eq!(
        app.sessions.current.index_build.indexed_bytes.get(),
        7,
        "from the chunk that was already running, not a second one"
    );
}

/// The loop's own step waits for the running chunk rather than dispatching
/// beside it. Same observation as the key above, from the other caller: a step
/// that dispatched while a chunk was running would overwrite the record and
/// drop that chunk's receiver.
#[test]
fn test_index_pump_waits() {
    let mut app = connected_working();
    app.sessions.persistence.snapshot = Some(Arc::new(MockSnapshot {
        lines: Vec::new(),
        log_bytes: 20 * 1024 * 1024,
        truncated: false,
        skipped: 0,
        window_lines: vec![TranscriptLine::User("tail".into())],
        window_start: 0,
        windows: Vec::new(),
        index_steps: 1,
        index_calls: AtomicU32::new(0),
    }));
    app.enter_search_view("tail");
    app.sessions.current.index_build.indexing.set(true);
    let (tx, rx) = channel();
    *app.sessions
        .current
        .index_build
        .pending_index_chunk
        .borrow_mut() = Some(PendingIndexChunk::new(rx));

    assert!(
        !app.pump_index_chunk(),
        "nothing landed yet, so nothing moved"
    );
    assert!(
        tx.send(IndexProgress {
            indexed_bytes: 3,
            total_bytes: 9,
            done: false,
        })
        .is_ok(),
        "the running chunk still has its receiver"
    );
}
