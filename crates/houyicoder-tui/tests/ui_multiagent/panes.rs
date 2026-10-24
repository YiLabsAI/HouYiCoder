use super::*;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use houyicoder_tui::composition::app;
use houyicoder_tui::keys::handle_working;
use houyicoder_tui::records::TranscriptLine;
use houyicoder_tui::state::Pane;
use houyicoder_tui::state::Screen;

/// A delegation in the transcript must not swallow Ctrl+O for the blocks
/// that follow it. The delegation runs first, then the parent answers with
/// reasoning, so the reasoning summary is the latest expandable block and the
/// key belongs to it. Delegation expand used to answer for every Ctrl+O
/// whatever the cursor pointed at, which left reasoning expandable by mouse
/// and impossible to collapse by keyboard. Only the real terminal proves the
/// key reaches the routing at all — a unit test calling the handler directly
/// skips the dispatch this pins.
#[test]
#[ignore]
fn test_multi_ctrl_o_thought() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth is in src/auth"}],
        [{"type":"Reasoning","text":"weighing the auth options"},{"type":"Text","text":"all done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("Thoughtfor", RENDER_TIMEOUT * 3),
        "the reasoning summary should render after the delegation:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for_compact("weighingtheauthoptions", RENDER_TIMEOUT),
        "Ctrl+O should expand the reasoning, not the earlier delegation:\n{}",
        s.output()
    );
    assert!(
        !s.output_compact().contains("childtranscript"),
        "the delegation should stay collapsed:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for_compact("expand)", RENDER_TIMEOUT),
        "a second Ctrl+O should offer expand again:\n{}",
        s.output()
    );
    assert!(
        !s.output_compact().contains("weighingtheauthoptions"),
        "collapsing should drop the reasoning body:\n{}",
        s.output()
    );
}

/// Two consecutive foreground delegations produce two independent
/// fold-groups, each carrying its own child summary. Proves per-child
/// rendering (not a merged or last-writer-wins fold).
#[test]
#[ignore]
fn test_foreground_two_folds_render() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"first","description":"first"}},
         {"type":"ToolCall","id":"toolu_2","name":"agent","input":{"subagent_type":"explore","prompt":"second","description":"second"}}],
        [{"type":"Text","text":"first-child-result"}],
        [{"type":"Text","text":"second-child-result"}],
        [{"type":"Text","text":"parent done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("delegate two");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("parentdone", RENDER_TIMEOUT * 3),
        "both children should complete + parent resume:\n{}",
        s.output()
    );
    let pc = s.output_compact();
    assert!(
        pc.contains("first-child-result") && pc.contains("second-child-result"),
        "both child summaries should render:\n{}",
        s.output()
    );
}

/// A long child text is truncated in the collapsed summary: the head shows
/// but the tail does not. Proves the summary caps at one row.
#[test]
#[ignore]
fn test_fold_summary_truncates() {
    let script = format!(
        r#"[
        [{{"type":"ToolCall","id":"toolu_1","name":"agent","input":{{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}}}],
        [{{"type":"Text","text":"{LONG_CHILD}"}}],
        [{{"type":"Text","text":"done"}}]
    ]"#
    );
    let mut s = pty_session_scripted(&script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    // The standing screen, not the byte stream: the stream writes cells at
    // absolute positions, so concatenating it drops characters wherever the
    // cursor jumped and a substring match there proves nothing about the row.
    let pc: String = s
        .screen()
        .contents()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        pc.contains("longchildanalysis"),
        "summary head should show the start:\n{}",
        s.screen().contents()
    );
    assert!(
        !pc.contains("trailingsentinel"),
        "collapsed summary should not show the tail:\n{}",
        s.screen().contents()
    );
}

/// The fold-group head shows the subagent_type label, not a generic
/// placeholder. Uses the registered "plan" type to prove the label is not
/// hard-coded to one value.
#[test]
#[ignore]
fn test_foreground_fold_shows_type() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"plan","prompt":"plan the work","description":"plan"}}],
        [{"type":"Text","text":"plan made"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("plan the work");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    assert!(
        s.output_compact().contains("plan:"),
        "fold head should show the subagent type:\n{}",
        s.output()
    );
}

/// Two delegations in one run with distinct subagent types each render
/// their own type label in the fold head (explore + plan), proving the
/// per-child type is not lost when multiple delegations land together.
#[test]
#[ignore]
fn test_two_folds_distinct_types() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"first","description":"first"}},
         {"type":"ToolCall","id":"toolu_2","name":"agent","input":{"subagent_type":"plan","prompt":"second","description":"second"}}],
        [{"type":"Text","text":"first-child"}],
        [{"type":"Text","text":"second-child"}],
        [{"type":"Text","text":"parent done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("delegate two");
    s.send_str("\r");
    assert!(s.wait_for_compact("parentdone", RENDER_TIMEOUT * 3));
    let pc = s.output_compact();
    assert!(
        pc.contains("explore:") && pc.contains("plan:"),
        "both fold heads should show their own type:\n{}",
        s.output()
    );
}

/// A short child text appears fully in the collapsed summary (no truncation
/// ellipsis for content under the one-line cap).
#[test]
#[ignore]
fn test_fold_short_summary_full() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"found it"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    assert!(
        s.output_compact().contains("foundit"),
        "short child text should appear fully in the summary:\n{}",
        s.output()
    );
}

/// The parent's final answer renders after the fold-group (the delegation
/// result precedes the parent's resume text in transcript order).
#[test]
#[ignore]
fn test_foreground_parent_after_fold() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"child found auth"}],
        [{"type":"Text","text":"parent final answer"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    assert!(
        s.output_compact().contains("parentfinalanswer"),
        "parent final answer should render after the fold:\n{}",
        s.output()
    );
    assert!(
        s.output_compact().contains("childfoundauth"),
        "child result should render in the fold:\n{}",
        s.output()
    );
}
/// The /agents pane lists this session's returned delegations once the
/// footer strip has dropped them: a returned delegation is durable history,
/// so the pane (the record surface) keeps it after the strip (the present
/// tense) drops it. Selectable, Enter opens the delegation's view.
#[test]
fn test_agents_pane_lists_returned() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Agents;
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
        color: None,
    });
    app.refresh_agents();
    assert_eq!(
        app.agents().rows.len(),
        1,
        "the returned delegation is listed"
    );
    assert!(app.agents().rows[0].loaded, "loaded fold detected");
    // Enter on the selected row opens that delegation's view.
    handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let view = app.ui.teammate_view.as_ref().expect("view opened");
    assert_eq!(view.child_sid, "c1");
    assert!(
        !view.transcript.is_empty(),
        "the loaded fold is copied into the view"
    );
}
/// The agents cursor steps and clamps at the bounds of the
/// returned-delegation list. Asserted through Enter, the pub-level action the
/// user takes: the child whose view opens is the row the cursor reached, so a
/// cursor that reads right while the action lands elsewhere fails here.
#[test]
fn test_agents_enter_follows_cursor() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Agents;
    for (sid, summary) in [("c1", "first"), ("c2", "second")] {
        app.push_transcript_line(TranscriptLine::Subagent {
            child_sid: sid.into(),
            subagent_type: "explore".into(),
            summary: summary.into(),
            prompt: String::new(),
            folded_transcript: vec![TranscriptLine::Agent("reply".into())],
            color: None,
        });
    }
    let v = app.transcript().revision();
    let rows = app.transcript().lines().to_vec();
    app.agents_mut().refresh(&rows, v);
    handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
    );
    assert_eq!(
        app.ui.teammate_view.as_ref().map(|v| v.child_sid.as_str()),
        Some("c2"),
        "the newest delegation leads the list"
    );
    app.move_agents_cursor(1);
    handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
    );
    assert_eq!(
        app.ui.teammate_view.as_ref().map(|v| v.child_sid.as_str()),
        Some("c1"),
        "the cursor stepped to the next row"
    );
    app.move_agents_cursor(1);
    app.move_agents_cursor(1);
    handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
    );
    assert_eq!(
        app.ui.teammate_view.as_ref().map(|v| v.child_sid.as_str()),
        Some("c1"),
        "the cursor clamps at the last row instead of wrapping"
    );
}

/// Exit a teammate view and drill into a DIFFERENT completed child. Proves
/// the view swaps to the second child's rows, not a stale first-child or
/// parent snapshot. Slow, ignored by default.
#[test]
#[ignore]
fn test_exit_enter_another_child() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"first","description":"first"}},
         {"type":"ToolCall","id":"toolu_2","name":"agent","input":{"subagent_type":"plan","prompt":"second","description":"second"}}],
        [{"type":"Text","text":"first-child-result"}],
        [{"type":"Text","text":"second-child-result"}],
        [{"type":"Text","text":"parent done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("delegate two");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("second-child-result", RENDER_TIMEOUT * 2),
        "both delegations should complete:\n{}",
        s.output()
    );
    // Drill into the most recent (second) delegation.
    s.send_str("\r");
    assert!(
        s.wait_for_plain("Viewing", RENDER_TIMEOUT),
        "teammate view should open:\n{}",
        s.output()
    );
    assert!(
        s.output_compact().contains("second-child-result"),
        "view should show the second child's content:\n{}",
        s.output()
    );
    // Exit back to the parent; both fold-groups repaint + banner gone.
    s.clear_output();
    s.send_key(&Key::ShiftDown);
    assert!(
        s.wait_for_compact("first-child-result", RENDER_TIMEOUT),
        "exit should restore the parent with both fold-groups:\n{}",
        s.output()
    );
    assert!(
        !s.output_compact().contains("Viewing"),
        "exit should clear the banner:\n{}",
        s.output()
    );
}

/// Exit the teammate view back to the parent and confirm the parent
/// transcript content (not the empty-input placeholder) repaints. The old
/// exit assertions used the placeholder, rendered in both views, a false
/// green. Slow, ignored by default.
#[test]
#[ignore]
fn test_exit_to_parent_repaints() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth is in src/auth"}],
        [{"type":"Text","text":"the auth module is in src/auth"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find the auth module");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+o", RENDER_TIMEOUT * 2));
    s.send_str("\r");
    assert!(s.wait_for_plain("Viewing", RENDER_TIMEOUT));
    s.clear_output();
    s.send_key(&Key::ShiftDown);
    assert!(
        s.wait_for_compact("authmoduleisinsrc/auth", RENDER_TIMEOUT),
        "exit should repaint the parent's delegation row:\n{}",
        s.output()
    );
    assert!(
        !s.output_compact().contains("Viewing"),
        "exit should clear the teammate banner:\n{}",
        s.output()
    );
}

/// The /agents journey on the real terminal: the pane lists the delegatable
/// types, the arrows move the highlight, Enter opens a type's detail, Esc
/// returns to the list, and the next Esc closes the pane. The routing order
/// is what this covers: while a detail is open the scroll keys and Esc belong
/// to it, not to the list behind it. Slow, ignored by default.
///
/// Read the reconstructed screen rather than the byte stream: a repaint sends
/// only the cells that changed, so moving the highlight re-sends the cursor
/// marker alone and the row's own text never crosses the stream a second time.
#[test]
#[ignore]
fn test_agents_pane_journey() {
    let mut s = common::PtySession::launch();
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "should reach login:\n{}",
        s.output()
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen:\n{}",
        s.output()
    );
    common::run_slash_command(&mut s, "agents");
    assert!(
        s.wait_for_screen("Available types", RENDER_TIMEOUT),
        "/agents should open the pane over the transcript:\n{}",
        s.output()
    );
    let listed = s.screen().contents();
    for subagent_type in ["code-guide", "explore", "general-purpose", "plan", "verify"] {
        assert!(
            listed.contains(subagent_type),
            "the pane should list the {subagent_type} type:\n{}",
            s.output()
        );
    }
    assert!(
        listed.contains("\u{25b6} - code-guide"),
        "the highlight starts on the first type:\n{}",
        s.output()
    );
    let text = s.screen().contents();
    let lines: Vec<&str> = text.lines().collect();
    let head = lines
        .iter()
        .rposition(|line| line.trim_start().starts_with("Agents"))
        .expect("the pane opens with its head line");
    assert!(
        lines[head].contains("types"),
        "the head counts the types it lists:\n{}",
        s.output()
    );
    let section = lines
        .iter()
        .position(|line| line.contains("Available types"))
        .expect("the section heading is on screen");
    assert_eq!(
        section,
        head + 1,
        "the head holds one line, so the body starts under it:\n{}",
        s.output()
    );
    s.send_key(&Key::Down);
    assert!(
        s.wait_for_screen("\u{25b6} - explore", RENDER_TIMEOUT),
        "down should move the highlight to the next type:\n{}",
        s.output()
    );
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_screen("When to use:", RENDER_TIMEOUT),
        "enter should open the highlighted type's detail:\n{}",
        s.output()
    );
    let detail = s.screen().contents();
    let lines: Vec<&str> = detail.lines().collect();
    let section = lines
        .iter()
        .position(|line| line.contains("Available for delegation"))
        .expect("the detail opens with its section row");
    assert!(
        lines[section - 1].contains("explore"),
        "the detail answers the row the highlight stands on:\n{}",
        s.output()
    );
    for field in ["Tools:", "System prompt"] {
        assert!(
            detail.contains(field),
            "the detail should show the {field} field:\n{}",
            s.output()
        );
    }
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for_screen("Available types", RENDER_TIMEOUT),
        "esc should return to the list, not close the pane:\n{}",
        s.output()
    );
    assert!(
        !s.screen().contents().contains("When to use:"),
        "the detail should be gone:\n{}",
        s.output()
    );
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for_screen("for commands", RENDER_TIMEOUT),
        "the list's esc should close the pane and bring the input box back:\n{}",
        s.output()
    );
}
