/// Regression: when an approval card is pending, prior transcript content
/// (a /context grid block and a user "hi" line near the tail) must stay
/// visible, not be erased. The old layout painted the card as a
/// bottom-aligned Clear-over-rect over the transcript area, which wiped the
/// last 13 rows every frame -- so wheel-scrolling up to re-read hi or the
/// grid showed them blanked. The card now occupies a reserved layout region
/// below the transcript, so the transcript keeps its own rows.
#[test]
fn test_approval_keeps_prior_content() {
    use crate::composition;
    use crate::records::TranscriptLine;
    use crate::state::{Approval, Screen};
    use crate::test_support::render_text;
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![
        TranscriptLine::ContextGrid(composition::context_view()),
        TranscriptLine::User("hi".to_string()),
    ];
    app.approval = Some(Approval {
        tool: "bash".to_string(),
        args: r#"{"command":"ls -la"}"#.to_string(),
        reason: "agent wants to run this tool".to_string(),
        selected: 0,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("hi"),
        "user line must stay visible with approval pending:\n{out}"
    );
    assert!(
        out.contains("Do you want to proceed?"),
        "the approval card must still render in its own region:\n{out}"
    );
}

/// Reasoning folds into the "thought for Ns (ctrl+o)" line
/// BELOW the answer — it does NOT render a separate ✻ thinking row ABOVE the
/// answer. A transcript with a Thinking line (reasoning) followed by an Agent
/// line (answer) must render the answer, and must NOT surface the ✻ thinking
/// row above it. Regression guard: an earlier fix removed the live content
/// echo but left the durable Thinking line rendering above the answer.
#[test]
fn test_thinking_not_above_answer() {
    use crate::composition;
    use crate::records::TranscriptLine;
    use crate::state::Screen;
    use crate::test_support::render_text;

    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![
        TranscriptLine::User("hi".to_string()),
        TranscriptLine::Thinking {
            text: "the user said hi, respond briefly".into(),
        },
        TranscriptLine::Agent("Hi! How can I help?".into()),
    ];
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Hi! How can I help?"),
        "answer must render: {out}"
    );
    assert!(
        !out.contains("✻ thinking"),
        "no ✻ thinking row above the answer (folded into thought-for line):\n{out}"
    );
    assert!(
        !out.contains("the user said hi"),
        "reasoning content must not surface above the answer:\n{out}"
    );
}

/// Turn-boundary fold: a completed turn's consecutive tool calls collapse
/// into one dim summary row. Collapsed view shows the summary and hides the
/// individual tool call chips; Ctrl+O and click both toggle expansion.
/// The desync check verifies the count path (fold_aware_rows) matches the
/// render path (draw_transcript rows.len()) in both collapsed and expanded
/// states.
#[test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
fn test_fold_collapse_expand_toggle() {
    use crate::app;
    use crate::composition;
    use crate::keys;
    use crate::records::{ToolOutcome, TranscriptLine};
    use crate::state::Screen;
    use crate::test_support::render_text;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };

    fn tcall(cid: &str, name: &str, brief: &str, oc: ToolOutcome) -> TranscriptLine {
        TranscriptLine::Tool {
            name: name.to_string(),
            tool: name.to_string(),
            status: brief.to_string(),
            invocation: brief.to_string(),
            outcome: oc,
            call_id: cid.to_string(),
            body: String::new(),
            is_diff: false,
        }
    }

    fn tresult(cid: &str, body: &str, oc: ToolOutcome) -> TranscriptLine {
        TranscriptLine::Tool {
            name: "result".to_string(),
            tool: "result".to_string(),
            status: String::new(),
            invocation: String::new(),
            outcome: oc,
            call_id: cid.to_string(),
            body: body.to_string(),
            is_diff: false,
        }
    }

    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![
        TranscriptLine::User("hi".to_string()),
        tcall("c1", "bash", "ls -la", ToolOutcome::Success),
        tresult("c1", "done", ToolOutcome::Success),
        tcall("c2", "read", "a.rs", ToolOutcome::Success),
        tresult("c2", "content", ToolOutcome::Success),
        TranscriptLine::Agent("all done".into()),
    ];

    // --- Collapsed state ---
    let out = render_text(&app, 80, 24);
    let count = app.transcript_display_rows();
    let rendered = app.transcript_scroll.total.get();
    assert_eq!(
        count, rendered,
        "desync collapsed: count={count} rendered={rendered}"
    );
    assert!(
        out.contains("Read 1 file"),
        "summary should be present: {out}"
    );
    assert!(
        out.contains("listed 1 directory"),
        "summary should be present: {out}"
    );
    assert!(
        !out.contains("Bash(ls"),
        "collapsed should hide tool calls: {out}"
    );
    assert!(
        !out.contains("Read(a"),
        "collapsed should hide tool calls: {out}"
    );
    // The ⎿ hint row under the summary uses the project-standard 2-space
    // gutter ("  ⎿  ", matching records.rs INTERRUPTED_NOTICE + markers.rs
    // result rows + the canonical '  ⎿  '). Pin so a future edit can't silently drop
    // to one trailing space (the regression that reads as cramped + ugly).
    assert!(
        out.contains("  ⎿  "),
        "hint gutter uses the 2-space ⎿  standard: {out}"
    );

    // --- Ctrl+O on summary row expands ---
    let fold_ri = app
        .last_row_fold_keys
        .borrow()
        .iter()
        .position(|k| k.is_some())
        .expect("a fold row should exist");
    let rect = app.transcript_rect.get();
    let fold_y = rect.y + fold_ri as u16;
    let total = app.transcript_scroll.total.get();
    let scroll_top = app.transcript_scroll.top_offset(total);
    let _ = fold_y;
    app.selection.start(0, scroll_top + fold_ri);
    keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
    );
    assert!(
        app.expanded_fold_groups.contains("c1#0"),
        "group should be expanded after Ctrl+O"
    );

    let out = render_text(&app, 80, 24);
    let count = app.transcript_display_rows();
    let rendered = app.transcript_scroll.total.get();
    assert_eq!(
        count, rendered,
        "desync expanded: count={count} rendered={rendered}"
    );
    assert!(
        out.contains("Bash(ls"),
        "expanded should show tool calls: {out}"
    );
    assert!(
        out.contains("Read(a"),
        "expanded should show tool calls: {out}"
    );
    assert!(
        out.contains("listed 1 directory"),
        "expanded keeps the summary as a collapse-handle header: {out}"
    );

    // --- Click on summary row toggles to expanded (same as Ctrl+O) ---
    app.expanded_fold_groups.clear();
    render_text(&app, 80, 24);
    let fold_ri = app
        .last_row_fold_keys
        .borrow()
        .iter()
        .position(|k| k.is_some())
        .expect("a fold row should exist");
    let rect = app.transcript_rect.get();
    let fold_y = rect.y + fold_ri as u16;
    app::handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: fold_y,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(
        app.expanded_fold_groups.contains("c1#0"),
        "click on summary should expand"
    );

    // --- Click on collapse hint toggles back to collapsed ---
    render_text(&app, 80, 24);
    let fold_ri = app
        .last_row_fold_keys
        .borrow()
        .iter()
        .position(|k| k.is_some())
        .expect("a fold row should exist");
    let rect = app.transcript_rect.get();
    let fold_y = rect.y + fold_ri as u16;
    app::handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: fold_y,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(
        !app.expanded_fold_groups.contains("c1#0"),
        "click on collapse hint should collapse"
    );
}

/// Regression for the busy-state count/render gap: the count path
/// (fold_aware_rows) must include the trailing live-preview and spinner rows
/// that draw after the slot region, else count falls below the rendered total
/// while the agent is busy or a reply is streaming. The non-busy desync
/// checks in test_fold_collapse_expand_toggle only cover the slot region.
#[test]
fn test_busy_count_matches_render() {
    use crate::composition;
    use crate::records::TranscriptLine;
    use crate::state::Screen;
    use crate::test_support::render_text;
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![
        TranscriptLine::User("hi".to_string()),
        TranscriptLine::Agent("working".to_string()),
    ];
    // All three trailing blocks active: live reasoning, live assistant, spinner.
    app.agent_busy = true;
    app.run_started = Some(std::time::Instant::now());
    app.live_active = true;
    app.live_reasoning_text = "reasoning about the task".to_string();
    app.live_assistant_text = "partial answer streaming".to_string();
    let _out = render_text(&app, 80, 24);
    let count = app.transcript_display_rows();
    let rendered = app.transcript_scroll.total.get();
    assert_eq!(
        count, rendered,
        "desync busy: count={count} rendered={rendered}"
    );

    // Spinner only (no live text yet): still one trailing row plus spacer.
    app.live_reasoning_text.clear();
    app.live_assistant_text.clear();
    let _out = render_text(&app, 80, 24);
    let count = app.transcript_display_rows();
    let rendered = app.transcript_scroll.total.get();
    assert_eq!(
        count, rendered,
        "desync spinner-only: count={count} rendered={rendered}"
    );
}

/// Markdown rendering: assistant text with ##, **, and backticks must
/// appear in the rendered output WITHOUT the raw syntax. The display shows
/// styled text (bold headers, colored code); the copy buffer has clean
/// text. This is the P-B fix: raw markdown is no longer emitted literally.
#[test]
fn test_markdown_strips_syntax() {
    use crate::composition;
    use crate::records::TranscriptLine;
    use crate::state::Screen;
    use crate::test_support::render_text;
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![TranscriptLine::Agent(
        "## Header\n\nSome **bold** and `code` here.".to_string(),
    )];
    let out = render_text(&app, 80, 24);
    assert!(
        !out.contains("##"),
        "heading syntax must be stripped:\n{out}"
    );
    assert!(!out.contains("**"), "bold syntax must be stripped:\n{out}");
    assert!(
        !out.contains("`"),
        "backtick syntax must be stripped:\n{out}"
    );
    assert!(
        out.contains("Header"),
        "heading text must be present:\n{out}"
    );
    assert!(out.contains("bold"), "bold text must be present:\n{out}");
    assert!(out.contains("code"), "code text must be present:\n{out}");
}

/// Full-row copy: the copy buffer (last_all_rows) must contain the FULL
/// transcript, not just the visible slice. When the user selects within
/// the visible area, copy must extract text from the correct rows via
/// scroll-offset mapping. This is the P-A fix: content is no longer lost
/// when the selection extends past the viewport edge.
#[test]
fn test_copy_preserves_scrolled_content() {
    use crate::composition;
    use crate::records::TranscriptLine;
    use crate::selection::{Selection, extract_text};
    use crate::state::Screen;
    use crate::test_support::render_text;
    use ratatui::layout::Rect;
    let mut app = composition::app();
    app.screen = Screen::Working;
    // 30 transcript lines; visible area is ~20 rows.
    let lines: Vec<TranscriptLine> = (0..30)
        .map(|i| TranscriptLine::Agent(format!("reply line {i}")))
        .collect();
    app.transcript = lines;
    let _out = render_text(&app, 80, 24);
    // last_all_rows should have all 30 lines (plus spacers), not just the
    // ~20 visible ones.
    let all_rows = app.last_all_rows.borrow();
    assert!(
        all_rows.len() > 25,
        "last_all_rows must contain the full transcript, got {} rows",
        all_rows.len()
    );
    // Selection at screen row 0..1 maps to scroll_top + 0..1 in the full
    // rows. Verify the extracted text comes from the scrolled position.
    let rect = app.transcript_rect.get();
    let total = app.transcript_scroll.total.get();
    let top = app.transcript_scroll.top_offset(total);
    let mut sel = Selection::default();
    sel.start(0, top);
    sel.update(79, top + 1);
    sel.finish();
    let text = extract_text(&all_rows, rect, &sel);
    assert!(
        !text.is_empty(),
        "copy must produce text from the full row set"
    );
    assert!(
        text.contains("reply line"),
        "extracted text must contain transcript content, got: {text}"
    );
    let _ = Rect::default();
}

/// Adversarial fold test: two separate fold groups (text breaks them). Click
/// group A's summary to expand it. Group B must stay collapsed — not expand
/// too. Reproduces the user report "click one gray summary, other folded
/// summaries also expand." If the keys are unique per group (first call_id)
/// + the toggle/display are per-key, only A expands.
#[test]
fn test_fold_expand_one_only() {
    use crate::composition;
    use crate::records::{ToolOutcome, TranscriptLine};
    use crate::state::Screen;
    use crate::test_support::render_text;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    fn tcall(cid: &str, name: &str, brief: &str, oc: ToolOutcome) -> TranscriptLine {
        TranscriptLine::Tool {
            name: name.to_string(),
            tool: name.to_string(),
            status: brief.to_string(),
            invocation: brief.to_string(),
            outcome: oc,
            call_id: cid.to_string(),
            body: String::new(),
            is_diff: false,
        }
    }
    fn tresult(cid: &str, body: &str, oc: ToolOutcome) -> TranscriptLine {
        TranscriptLine::Tool {
            name: "result".to_string(),
            tool: "result".to_string(),
            status: String::new(),
            invocation: String::new(),
            outcome: oc,
            call_id: cid.to_string(),
            body: body.to_string(),
            is_diff: false,
        }
    }

    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![
        TranscriptLine::User("hi".to_string()),
        // Group A: two consecutive bash calls, keyed by c1.
        tcall("c1", "bash", "ls -la", ToolOutcome::Success),
        tresult("c1", "done", ToolOutcome::Success),
        tcall("c2", "bash", "ls -la", ToolOutcome::Success),
        tresult("c2", "done", ToolOutcome::Success),
        TranscriptLine::Agent("middle text".into()), // breaks the group
        // Group B: two more bash calls, keyed by c3.
        tcall("c3", "bash", "find .", ToolOutcome::Success),
        tresult("c3", "done", ToolOutcome::Success),
        tcall("c4", "bash", "find .", ToolOutcome::Success),
        tresult("c4", "done", ToolOutcome::Success),
    ];
    render_text(&app, 80, 24);
    // Find the first fold-summary row (group A, key=c1) and click it.
    let rect = app.transcript_rect.get();
    let fold_ri = app
        .last_row_fold_keys
        .borrow()
        .iter()
        .position(|k| k.is_some())
        .expect("a fold row should exist");
    assert_eq!(
        app.last_row_fold_keys.borrow()[fold_ri].as_deref(),
        Some("c1#0"),
        "first fold row should be group A (key c1#0)"
    );
    crate::app::handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y + fold_ri as u16,
            modifiers: KeyModifiers::NONE,
        },
    );
    // Only c1 expanded, not c3.
    assert!(app.expanded_fold_groups.contains("c1#0"), "c1 expanded");
    assert!(
        !app.expanded_fold_groups.contains("c3"),
        "c3 must NOT expand when c1 is clicked: {:?}",
        app.expanded_fold_groups
    );
    // Render: group A's calls visible (the ⏺ Bash(ls -la) chip expands),
    // group B's calls hidden — collapsed shows only the summary + ⎿ $find .
    // hint preview, NOT the ⏺ Bash(find .) chip. The hint intentionally
    // reveals the last command (that's its job), so distinguish collapsed
    // from expanded by the chip glyph, not the bare command substring.
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("Bash(ls -la)"),
        "group A expanded shows its chip: {out}"
    );
    assert!(
        !out.contains("Bash(find .)"),
        "group B collapsed hides its chip (hint ⎿ $find . is ok): {out}"
    );
}

/// Selection anchor stays in content-row space, not on border rows or blank
/// tail rows. The rect can include blank rows below the content tail and can
/// be one frame stale. The Down handler clamps the click to the last visible
/// content row, and the overlay skips rows beyond the visible content.
/// Verifies: (1) anchor content row maps to a real row, (2) drag past the
/// bottom edge keeps the focus on a content row, (3) overlay never paints on
/// the border row or blank tail rows.
#[test]
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
fn test_anchor_clamped_to_content() {
    use crate::composition;
    use crate::selection::RecordingClipboard;
    use crate::state::Screen;
    use crate::test_support::{render_buffer, render_text};
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::style::Color;
    use std::sync::{Arc, Mutex};

    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![
        crate::records::TranscriptLine::User("alpha bravo charlie".into()),
        crate::records::TranscriptLine::Agent("delta echo foxtrot".into()),
    ];
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    app.clipboard = Arc::new(RecordingClipboard {
        captured: captured.clone(),
    });
    let _out = render_text(&app, 80, 24);
    let rect = app.transcript_rect.get();
    assert!(rect.height > 1, "transcript rect must have height");
    let bottom = rect.y + rect.height - 1;
    let border_y = rect.y + rect.height;
    let total = app.transcript_scroll.total.get();
    let scroll_top = app.transcript_scroll.top_offset(total);
    let visible = total.saturating_sub(scroll_top).min(rect.height as usize);
    assert!(
        visible < rect.height as usize,
        "test needs content that does NOT fill the viewport (visible={} < height={})",
        visible,
        rect.height
    );
    let last_content_y = rect.y + visible as u16 - 1;

    // Click at the very bottom of the transcript rect — a blank tail row.
    crate::app::handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: bottom,
            modifiers: KeyModifiers::NONE,
        },
    );
    // Anchor must be clamped to the last content row, not the blank tail.
    let (_, acr) = app.selection.anchor.expect("anchor set after click");
    let last_content_row = scroll_top + (last_content_y as usize) - rect.y as usize;
    assert_eq!(
        acr, last_content_row,
        "anchor must be clamped to the last content row, not the blank tail"
    );
    assert!(
        acr < app.last_all_rows.borrow().len(),
        "anchor content row {acr} must index a real row in last_all_rows (len={})",
        app.last_all_rows.borrow().len()
    );

    // Drag past the bottom edge into the input border area.
    crate::app::handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: rect.x + 40,
            row: border_y,
            modifiers: KeyModifiers::NONE,
        },
    );
    let (_, fcr) = app.selection.focus.expect("focus set after drag");
    assert!(
        fcr <= last_content_row,
        "focus row {fcr} must be clamped to the last content row {last_content_row}"
    );

    // Release auto-copies (direct paste works); the selection was clamped to
    // content rows, not blank tail rows.
    crate::app::handle_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: rect.x + 40,
            row: border_y,
            modifiers: KeyModifiers::NONE,
        },
    );

    // The copied text must be real content (the selection was clamped to
    // content rows, not blank tail rows).
    let got = captured.lock().expect("captured").clone();
    assert_eq!(got.len(), 1, "one copy on mouse-up: {got:?}");
    assert!(
        got[0].contains("alpha")
            || got[0].contains("bravo")
            || got[0].contains("charlie")
            || got[0].contains("delta")
            || got[0].contains("echo")
            || got[0].contains("foxtrot"),
        "copied text must be transcript content, got: {got:?}"
    );

    // Render with a selection spanning into the blank tail and verify the
    // overlay never paints on border or blank-tail rows.
    app.selection.start(rect.x, last_content_row);
    app.selection.update(
        rect.x + 10,
        scroll_top + (bottom as usize) - rect.y as usize,
    );
    app.selection.finish();
    let buf = render_buffer(&app, 80, 24);
    // Border row must have no selection bg.
    if border_y < 24 {
        for x in 0..80 {
            let cell = buf.cell((x, border_y)).expect("border cell");
            assert!(
                cell.bg == Color::Reset,
                "overlay must not paint on border row {border_y} col {x}: bg {:?}",
                cell.bg
            );
        }
    }
    // Blank tail rows (between content and border) must have no selection bg.
    if visible < rect.height as usize {
        for y in (last_content_y + 1)..border_y {
            for x in rect.x..rect.x + rect.width.min(10) {
                let cell = buf.cell((x, y)).expect("tail cell");
                assert!(
                    cell.bg == Color::Reset,
                    "overlay must not paint on blank tail row {y} col {x}: bg {:?}",
                    cell.bg
                );
            }
        }
    }
}

/// The checklist renders inline at the transcript tail, so the count path
/// (fold_aware_rows, via live_trailing_row_count) must include its rows plus
/// the leading spacer and match the rendered total in the collapsed, expanded,
/// and all-done states — otherwise the scroll offset drifts past real rows.
#[test]
fn test_todo_count_matches_render() {
    use crate::composition;
    use crate::records::TranscriptLine;
    use crate::state::Screen;
    use crate::test_support::render_text;
    use crate::todo_view::{TodoStatus, TodoView};

    fn item(content: &str, status: TodoStatus) -> TodoView {
        let active_form = (status == TodoStatus::InProgress).then(|| format!("doing {content}"));
        TodoView {
            content: content.into(),
            status,
            active_form,
        }
    }

    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![TranscriptLine::User("go".to_string())];
    app.todos_cache = vec![
        item("a", TodoStatus::Completed),
        item("b", TodoStatus::InProgress),
        item("c", TodoStatus::Pending),
        item("d", TodoStatus::Pending),
    ];

    // Collapsed: two visible items plus a hidden-count footer.
    let _out = render_text(&app, 80, 24);
    assert_eq!(
        app.transcript_display_rows(),
        app.transcript_scroll.total.get(),
        "desync collapsed checklist"
    );

    // Expanded: every item renders.
    app.todo_expanded = true;
    let _out = render_text(&app, 80, 24);
    assert_eq!(
        app.transcript_display_rows(),
        app.transcript_scroll.total.get(),
        "desync expanded checklist"
    );

    // All done: a single summary line.
    app.todo_expanded = false;
    app.todos_cache = vec![
        item("a", TodoStatus::Completed),
        item("b", TodoStatus::Completed),
    ];
    let _out = render_text(&app, 80, 24);
    assert_eq!(
        app.transcript_display_rows(),
        app.transcript_scroll.total.get(),
        "desync all-done checklist"
    );
}

/// The checklist items are selectable transcript rows: the row set the
/// selection/copy path reads carries the glyph-plus-label text as plain
/// content (no non-selectable widget tag), so a drag can copy the checklist.
#[test]
fn test_todo_rows_are_selectable() {
    use crate::composition;
    use crate::records::TranscriptLine;
    use crate::selection::is_non_selectable;
    use crate::state::Screen;
    use crate::test_support::render_text;
    use crate::todo_view::{TodoStatus, TodoView};

    let mut app = composition::app();
    app.screen = Screen::Working;
    app.transcript = vec![TranscriptLine::User("go".to_string())];
    app.todos_cache = vec![
        TodoView {
            content: "write code".into(),
            status: TodoStatus::InProgress,
            active_form: Some("writing code".into()),
        },
        TodoView {
            content: "ship it".into(),
            status: TodoStatus::Pending,
            active_form: None,
        },
    ];
    let _out = render_text(&app, 80, 24);
    let rows = app.last_all_rows.borrow();
    let todo_rows: Vec<&(u8, String)> = rows
        .iter()
        .filter(|(_, s)| s.contains("writing code") || s.contains("ship it"))
        .collect();
    assert!(
        todo_rows.len() >= 2,
        "checklist items must appear in the selectable row set: {rows:?}"
    );
    for (tag, text) in todo_rows {
        assert!(
            !is_non_selectable(*tag),
            "checklist row must be selectable, got tag {tag} for {text:?}"
        );
    }
}

/// Eager tool callers reuse one call_id across distinct calls. Two groups
/// sharing a call_id must get distinct expanded-set keys (call_id#ordinal) so
/// expanding one does not expand the other — a bare call_id collides on the
/// HashSet.
#[test]
fn test_distinct_keys_callid_reuse() {
    use crate::fold::{DisplaySlot, compute_fold_groups, display_slots};
    use crate::records::{ToolOutcome, TranscriptLine};
    fn tcall(cid: &str, brief: &str, oc: ToolOutcome) -> TranscriptLine {
        TranscriptLine::Tool {
            name: "bash".to_string(),
            tool: "bash".to_string(),
            status: brief.to_string(),
            invocation: brief.to_string(),
            outcome: oc,
            call_id: cid.to_string(),
            body: String::new(),
            is_diff: false,
        }
    }
    fn tresult(cid: &str, oc: ToolOutcome) -> TranscriptLine {
        TranscriptLine::Tool {
            name: "result".to_string(),
            tool: "result".to_string(),
            status: String::new(),
            invocation: String::new(),
            outcome: oc,
            call_id: cid.to_string(),
            body: String::new(),
            is_diff: false,
        }
    }
    let t = vec![
        TranscriptLine::User("turn one".into()),
        tcall("c1", "ls", ToolOutcome::Success),
        tresult("c1", ToolOutcome::Success),
        TranscriptLine::Agent("mid".into()),
        // Same call_id c1, reused by an eager caller in turn two.
        tcall("c1", "find .", ToolOutcome::Success),
        tresult("c1", ToolOutcome::Success),
    ];
    let groups = compute_fold_groups(&t, false);
    assert_eq!(groups.len(), 2, "two groups (Agent line breaks the run)");
    assert_ne!(groups[0].key, groups[1].key, "same call_id, distinct keys");
    assert_eq!(groups[0].key, "c1#0");
    assert_eq!(groups[1].key, "c1#1");
    // Expanding only the first group leaves the second collapsed.
    let mut expanded = std::collections::HashSet::new();
    expanded.insert(groups[0].key.clone());
    let slots = display_slots(&t, false, &expanded, false);
    let summaries: Vec<_> = slots
        .iter()
        .filter(|s| matches!(s, DisplaySlot::Summary(_)))
        .collect();
    assert_eq!(
        summaries.len(),
        2,
        "both summaries render (group 1 header, group 2 collapsed)"
    );
}
