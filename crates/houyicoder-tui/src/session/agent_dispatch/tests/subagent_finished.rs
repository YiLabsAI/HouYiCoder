use super::*;
use crate::app::handle_mouse;
use crate::composition::app;
use crate::selection::RecordingClipboard;
use crate::state::App;
use crate::state::Screen;
use crate::state::ViewportMode;
use crate::test_harness::dump_buffer;
use crate::test_harness::render_buffer;
use crate::test_harness::render_text;
use crate::transcript::records::SubagentFinished;
use crate::transcript::records::SubagentOutcome;
use crate::transcript::records::TranscriptLine;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use std::sync::{Arc, Mutex};

/// An app on the working screen, where the transcript and the fleet strip
/// both draw.
fn working_app() -> App {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.viewport = ViewportMode::Working;
    app
}

/// One child status snapshot for the same delegation.
fn status(completed: Option<&str>, tool_uses: u32) -> SessionMessage {
    SessionMessage::Event(ServerEvent::AgentStatus {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        description: "map the auth path".into(),
        turn: 2,
        tokens: 4200,
        tool_uses,
        last_activity: None,
        completed: completed.map(str::to_string),
    })
}

/// One buffer row as the text the user sees.
fn row_text(buf: &Buffer, y: u16) -> String {
    let area = buf.area();
    (0..area.width)
        .map(|x| buf.cell((x, y)).expect("cell").symbol())
        .collect()
}

/// The rows of the buffer that draw a finished trace, so the count path and
/// the draw path are compared on what reached the screen.
fn drawn_trace_rows(buf: &Buffer) -> usize {
    (0..buf.area().height)
        .filter(|&y| row_text(buf, y).contains("Agent \""))
        .count()
}

/// The color of the mark on the trace row, found by its row text so the
/// fleet strip's own glyph cannot be mistaken for it.
fn trace_mark_color(buf: &Buffer) -> Color {
    let area = buf.area();
    for y in 0..area.height {
        if !row_text(buf, y).contains("Agent \"") {
            continue;
        }
        for x in 0..area.width {
            let cell = buf.cell((x, y)).expect("cell");
            if cell.symbol() == "\u{25cf}" {
                return cell.fg;
            }
        }
    }
    panic!("no finished trace on screen:\n{}", dump_buffer(buf));
}

/// A child that finishes leaves one row in the transcript naming the task and
/// its ending; while it runs, the transcript holds no such row. The strip
/// drops its row after the grace window, and this is what stays behind.
#[test]
fn test_finished_status_leaves_trace() {
    let mut app = working_app();
    app.handle_agent_message(status(None, 3));
    let running = render_text(&app, 100, 30);
    assert!(
        !running.contains("Agent \"map the auth path\" finished"),
        "no trace while the child runs:\n{running}"
    );
    app.handle_agent_message(status(Some("completed"), 3));
    let done = render_text(&app, 100, 30);
    assert!(
        done.contains("\u{25cf} Agent \"map the auth path\" finished"),
        "the trace names the task and its ending:\n{done}"
    );
    assert!(
        done.contains("3 tools"),
        "the trace reports the child's tool count:\n{done}"
    );
}

/// A repeated terminal status for the same child does not raise a second
/// trace: the row is written on the first landing and every echo after it
/// finds the entry already completed.
#[test]
fn test_repeat_completion_one_trace() {
    let mut app = working_app();
    app.handle_agent_message(status(None, 2));
    app.handle_agent_message(status(Some("completed"), 2));
    app.handle_agent_message(status(Some("completed"), 2));
    let done = render_text(&app, 100, 30);
    assert_eq!(
        done.matches("\u{25cf} Agent").count(),
        1,
        "one trace per child:\n{done}"
    );
}

/// A child first heard about in its terminal status was never watched
/// running, so the row carries no duration segment rather than a run of zero
/// length. The tool count is a real report and still shows.
#[test]
fn test_never_observed_no_duration() {
    let mut app = working_app();
    app.handle_agent_message(status(Some("completed"), 1));
    let done = render_text(&app, 100, 30);
    assert!(
        done.contains("\u{25cf} Agent \"map the auth path\" finished \u{00b7} 1 tool"),
        "no duration stands before the tool count:\n{done}"
    );
    assert!(
        !done.contains("0s"),
        "a run never observed claims no length:\n{done}"
    );
}

/// The mark carries the ending: success green, failure red, interruption
/// yellow, and an ending this build cannot name stays neutral. The row text
/// is identical across the outcomes, so only the cell color tells a reader
/// which happened -- a wrong color would report an ending nobody observed.
#[test]
fn test_trace_mark_matches_ending() {
    for (word, expected) in [
        ("completed", Color::Rgb(78, 186, 101)),
        ("turn_limit", Color::Rgb(78, 186, 101)),
        ("budget", Color::Rgb(78, 186, 101)),
        ("failed", Color::Rgb(255, 107, 128)),
        ("killed", Color::Yellow),
        ("some_future_word", Color::DarkGray),
    ] {
        let mut app = working_app();
        app.handle_agent_message(status(Some(word), 1));
        let color = trace_mark_color(&render_buffer(&app, 100, 30));
        assert_eq!(color, expected, "mark for a {word} child");
    }
}

/// A terminal word that lands after the child's fleet row left the strip
/// still leaves one trace and no new strip row. The entry that reported the
/// completion is gone by then, so a second word looks like the first word
/// about a new child; the transcript is the record that keeps one row for
/// one ending, and the strip does not reopen a row it already closed.
#[test]
fn test_absent_entry_one_trace() {
    let mut app = working_app();
    app.handle_agent_message(status(Some("completed"), 2));
    app.sessions.current.capabilities.fleet.entries.clear();
    app.handle_agent_message(status(Some("completed"), 2));
    let done = render_text(&app, 100, 30);
    assert_eq!(
        done.matches("\u{25cf} Agent").count(),
        1,
        "one trace per ending:\n{done}"
    );
    assert!(
        !done.contains("\u{00b7} finished \u{00b7}"),
        "the echo opens no completed strip row:\n{done}"
    );
}

/// Enter on a trace row opens the teammate view naming the child the cursor
/// is on, and the banner repeats the type the trace carried. The type has no
/// live fleet row to come from once that row left the strip, so a view that dropped
/// it would open as an anonymous agent.
#[test]
fn test_enter_opens_finished_child() {
    let mut app = working_app();
    for (sid, agent_type, summary) in [("c1", "explore", "first"), ("c2", "verify", "second")] {
        app.push_transcript_line(TranscriptLine::SubagentFinished(SubagentFinished {
            child_sid: sid.into(),
            subagent_type: agent_type.into(),
            description: summary.into(),
            outcome: SubagentOutcome::Finished,
            duration_ms: Some(1200),
            tool_uses: 2,
        }));
    }
    app.ui.selection.transcript.start(0, 0);
    assert!(app.enter_teammate_view(), "cursor on a trace row enters");
    let out = render_text(&app, 100, 30);
    assert!(
        out.contains("Viewing @explore"),
        "the cursor's child, not the newest:\n{out}"
    );
}

/// A transcript whose only line naming a child is its trace: with the cursor
/// past the tail, the fallback still opens that child, so the row is a way in
/// rather than a dead label.
#[test]
fn test_tail_fallback_opens_child() {
    let mut app = working_app();
    app.push_transcript_line(TranscriptLine::SubagentFinished(SubagentFinished {
        child_sid: "bg1".into(),
        subagent_type: "explore".into(),
        description: "background sweep".into(),
        outcome: SubagentOutcome::Finished,
        duration_ms: Some(4000),
        tool_uses: 9,
    }));
    app.ui.selection.transcript.start(999, 999);
    assert!(app.enter_teammate_view(), "the trace is a target");
    let out = render_text(&app, 100, 30);
    assert!(
        out.contains("Viewing @explore") && out.contains("background sweep"),
        "the trace names the view it opened:\n{out}"
    );
}

/// The footer strip's row for a child, found by its task summary, so a
/// transcript row that shares the summary cannot answer for it.
fn footer_row(out: &str) -> String {
    out.lines()
        .find(|line| line.contains("map the auth path") && line.contains("tokens"))
        .unwrap_or_else(|| panic!("no footer row on screen:\n{out}"))
        .trim_end()
        .to_string()
}

/// The footer names the ending its status reported. It used to read done for
/// every ending, so a failed or stopped child's footer row contradicted the
/// transcript row the reader could see in the same screen.
#[test]
fn test_footer_names_ending() {
    for (word, expected) in [("failed", "failed"), ("killed", "interrupted")] {
        let mut app = working_app();
        app.handle_agent_message(status(None, 2));
        app.handle_agent_message(status(Some(word), 2));
        let out = render_text(&app, 100, 30);
        let footer = footer_row(&out);
        assert!(
            footer.contains(expected),
            "a {word} child's footer row names its ending: {footer}"
        );
        assert!(
            !footer.contains("finished"),
            "and never borrows the word a successful child uses: {footer}"
        );
    }
}

/// One trace for the given task, with the ending and figures every width
/// test scans for.
fn trace(description: &str) -> SubagentFinished {
    SubagentFinished {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        description: description.into(),
        outcome: SubagentOutcome::Finished,
        duration_ms: Some(12_000),
        tool_uses: 7,
    }
}

/// The row of the rendered screen that draws the trace. The mark and the
/// agent word are found rather than the quote pair, because a row too narrow
/// for a name draws no quotes.
fn trace_row(app: &App, width: u16, height: u16) -> String {
    let out = render_text(app, width, height);
    out.lines()
        .find(|line| line.contains("\u{25cf} Agent"))
        .unwrap_or_else(|| panic!("no trace row on screen:\n{out}"))
        .trim_end()
        .to_string()
}

/// The trace fits the width it is drawn at. The name is the only segment of
/// no fixed length, so it gives way first: the ending and the measured
/// figures stay whole, because a row clipped at the right edge loses exactly
/// the words that say how the child ended.
#[test]
fn test_trace_row_fits_width() {
    let mut app = working_app();
    app.push_transcript_line(TranscriptLine::SubagentFinished(trace(
        "map every route the auth middleware guards and trace each handler",
    )));
    let row = trace_row(&app, 60, 20);
    assert!(row.contains("finished"), "the ending survives: {row:?}");
    assert!(row.contains("7 tools"), "the figures survive: {row:?}");
    assert!(
        row.contains('\u{2026}'),
        "the name is the segment that gives way: {row:?}"
    );
}

/// As the row loses width, the name gives columns up before the facts beside
/// it: the tool count leaves before the duration, and the duration before
/// the ending. The ending says how the child ended, which no clipped row may
/// hide.
#[test]
fn test_trace_keeps_figures() {
    let mut app = working_app();
    app.push_transcript_line(TranscriptLine::SubagentFinished(trace(
        "map every route the auth middleware guards and trace each handler",
    )));
    for width in [60, 40, 30] {
        let row = trace_row(&app, width, 20);
        assert!(row.contains("finished"), "width {width}: {row:?}");
        assert!(row.contains("12s"), "width {width}: {row:?}");
    }
    let tight = trace_row(&app, 30, 20);
    assert!(
        !tight.contains("7 tools"),
        "the tool count leaves before the name: {tight:?}"
    );
}

/// A row too short for a name drops the quote pair and keeps the ending
/// whole: an empty pair of quotes reads as a delegation with a blank task,
/// and a clipped ending reports an outcome nobody can finish reading.
#[test]
fn test_trace_short_width() {
    let mut app = working_app();
    app.push_transcript_line(TranscriptLine::SubagentFinished(trace(
        "map every route the auth middleware guards and trace each handler",
    )));
    let row = trace_row(&app, 18, 20);
    assert!(
        !row.contains("map"),
        "no name is drawn when the ending needs the row: {row:?}"
    );
    assert!(
        !row.contains("Agent \"\""),
        "no empty quoted name is drawn: {row:?}"
    );
    assert!(
        row.contains("finished"),
        "the whole ending is drawn: {row:?}"
    );
}

/// The band where the ending and the duration fit but no name column
/// remains: the row reads as the ending and figures, never as a quoted
/// blank name.
#[test]
fn test_trace_narrow_drops_quotes() {
    let mut app = working_app();
    app.push_transcript_line(TranscriptLine::SubagentFinished(trace(
        "map every route the auth middleware guards and trace each handler",
    )));
    for width in [25, 20, 18] {
        let row = trace_row(&app, width, 20);
        assert!(
            !row.contains("Agent \"\""),
            "width {width} draws no empty quoted name: {row:?}"
        );
        assert!(row.contains("finished"), "width {width}: {row:?}");
    }
    let narrow = trace_row(&app, 8, 20);
    assert!(
        !narrow.contains("Agent \"\""),
        "even 8 columns draw no empty quoted name: {narrow:?}"
    );
}

/// Every measurement is display columns: a name written in wide characters
/// takes two per character, and the cut lands on a character boundary.
#[test]
fn test_trace_wide_glyph_name() {
    let mut app = working_app();
    app.push_transcript_line(TranscriptLine::SubagentFinished(trace(
        "修复认证模块与路由的全部处理器",
    )));
    let row = trace_row(&app, 30, 20);
    assert!(
        row.contains('修') && row.contains('复'),
        "the name keeps its first characters: {row:?}"
    );
    assert!(row.contains('\u{2026}'), "the name is cut: {row:?}");
    assert!(row.contains("finished"), "the ending stays: {row:?}");
    assert!(row.contains("12s"), "the duration stays: {row:?}");
}

/// The expand key passes on a trace: the row names a child that has ended,
/// so there is no child transcript behind it to unfold. The key stays free
/// for the rows around it.
#[test]
fn test_expand_skips_trace() {
    let mut app = working_app();
    app.push_transcript_line(TranscriptLine::SubagentFinished(trace("find auth")));
    render_text(&app, 100, 30);
    app.ui.selection.transcript.start(0, 0);
    assert!(
        !app.toggle_subagent_expand(),
        "a finished child's trace has nothing to unfold"
    );
    assert!(
        app.ui.transcript_view.expanded.subagents.is_empty(),
        "no row was marked open by the key that passed"
    );
}

/// Two children whose tasks read the same are told apart by the row the
/// cursor is on: Enter opens the one under the cursor, not the newest whose
/// name matches. Both rows read alike on screen, so the opened child id is
/// the only observable that says which one the key reached.
#[test]
fn test_enter_opens_cursor_child() {
    let mut app = working_app();
    for sid in ["c1", "c2"] {
        app.push_transcript_line(TranscriptLine::SubagentFinished(SubagentFinished {
            child_sid: sid.into(),
            ..trace("find auth")
        }));
    }
    render_text(&app, 100, 30);
    app.ui.selection.transcript.start(0, 1);
    assert!(app.enter_teammate_view(), "the second row is a target");
    assert_eq!(
        app.ui.teammate_view.as_ref().map(|v| v.child_sid.as_str()),
        Some("c2"),
        "the row under the cursor, not the newest match"
    );
}

/// The trace rides the frame log, so a rebuild of the transcript rows keeps
/// it: the row is drawn from the log rather than from state a rebuild drops.
#[test]
fn test_trace_survives_rebuild() {
    let mut app = working_app();
    app.handle_agent_message(status(None, 2));
    app.handle_agent_message(status(Some("completed"), 2));
    app.rebuild_transcript();
    let out = render_text(&app, 100, 30);
    assert!(
        out.contains("\u{25cf} Agent \"map the auth path\" finished"),
        "the trace is still on screen after a rebuild:\n{out}"
    );
}

/// A task summary carrying a newline still draws as one row, and the count
/// path lands on that same one. A two-row draw counted as one would leave
/// every transcript offset below the trace drifted.
#[test]
fn test_trace_name_one_line() {
    let mut app = working_app();
    let line = TranscriptLine::SubagentFinished(trace("one\ntwo"));
    let rendered = line.render();
    assert_eq!(
        rendered.lines().count(),
        1,
        "the trace text is one line: {rendered:?}"
    );
    assert!(
        rendered.contains("one two"),
        "the newline reads as a space: {rendered:?}"
    );
    assert_eq!(app.line_display_rows(&line), 1, "counted as one row");
    app.push_transcript_line(line);
    let buf = render_buffer(&app, 60, 20);
    assert_eq!(
        drawn_trace_rows(&buf),
        1,
        "drawn as one row:\n{}",
        dump_buffer(&buf)
    );
    let out = render_text(&app, 60, 20);
    assert!(out.contains("finished"), "the ending stays visible:\n{out}");
}

/// The screen rows the finished traces draw on, top to bottom.
fn trace_rows(buf: &Buffer) -> Vec<u16> {
    let rows: Vec<u16> = (0..buf.area().height)
        .filter(|&y| row_text(buf, y).contains("Agent \""))
        .collect();
    assert!(!rows.is_empty(), "no trace row:\n{}", dump_buffer(buf));
    rows
}

/// One left-button event at a cell.
fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

/// The child the teammate view is showing, if any.
fn viewed_child(app: &App) -> Option<String> {
    app.ui.teammate_view.as_ref().map(|v| v.child_sid.clone())
}

/// A clean click on a finished trace opens the child that row names, not only
/// selecting its text. Two traces with different types and ids stand on
/// screen and the lower one is clicked, so a per-row key that arrived one row
/// off would open the child above it and fail here.
#[test]
fn test_click_trace_drills() {
    let mut app = working_app();
    for (sid, agent_type) in [("c1", "explore"), ("c2", "verify")] {
        app.push_transcript_line(TranscriptLine::SubagentFinished(SubagentFinished {
            child_sid: sid.into(),
            subagent_type: agent_type.into(),
            ..trace("find auth")
        }));
    }
    let buf = render_buffer(&app, 100, 30);
    let y = trace_rows(&buf)[1];
    let rect = app.ui.frame.layout.transcript_rect.get();
    let x = rect.x + 2;
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), x, y),
    );
    handle_mouse(&mut app, mouse(MouseEventKind::Up(MouseButton::Left), x, y));
    let out = render_text(&app, 100, 30);
    assert!(
        out.contains("Viewing @verify"),
        "the clicked row's child is the one on screen:\n{out}"
    );
    assert_eq!(
        viewed_child(&app).as_deref(),
        Some("c2"),
        "and the second trace opened the second child"
    );
}

/// A drag that starts on a finished trace copies the row's text and leaves
/// the reader in the parent, so selecting the trace never drills in.
#[test]
fn test_drag_trace_copies() {
    let mut app = working_app();
    app.push_transcript_line(TranscriptLine::SubagentFinished(trace("find auth")));
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    app.ui.clipboard = Arc::new(RecordingClipboard {
        captured: captured.clone(),
    });
    let buf = render_buffer(&app, 100, 30);
    let y = trace_rows(&buf)[0];
    let rect = app.ui.frame.layout.transcript_rect.get();
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), rect.x + 2, y),
    );
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), rect.x + 12, y),
    );
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), rect.x + 12, y),
    );
    assert!(
        viewed_child(&app).is_none(),
        "a drag selects the row's text instead of leaving the parent"
    );
    let copied = captured.lock().expect("captured").clone();
    assert_eq!(copied.len(), 1, "the release copies once: {copied:?}");
    assert!(
        copied[0].contains("Agent"),
        "the copied text is the trace row's: {copied:?}"
    );
}
