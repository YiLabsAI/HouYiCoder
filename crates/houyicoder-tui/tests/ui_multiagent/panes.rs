use super::*;

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

/// Two consecutive sync delegations produce two independent fold-groups,
/// each carrying its own child summary. Proves per-child rendering (not a
/// merged or last-writer-wins fold).
#[test]
#[ignore]
fn test_sync_two_folds_render() {
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
    let pc = s.output_compact();
    assert!(
        pc.contains("longchildanalysis"),
        "summary head should show the start:\n{}",
        s.output()
    );
    assert!(
        !pc.contains("trailingsentinel"),
        "collapsed summary should not show the tail:\n{}",
        s.output()
    );
}

/// The fold-group head shows the subagent_type label, not a generic
/// placeholder. Uses the registered "plan" type to prove the label is not
/// hard-coded to one value.
#[test]
#[ignore]
fn test_sync_fold_shows_type() {
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
fn test_sync_parent_after_fold() {
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
/// footer strip has retired them: a returned delegation is durable history,
/// so the pane (the record surface) keeps it after the strip (the present
/// tense) drops it. Selectable, Enter opens the delegation's view.
#[test]
fn test_agents_pane_lists_returned() {
    use houyicoder_tui::records::TranscriptLine;
    let mut app = houyicoder_tui::composition::app();
    app.screen = houyicoder_tui::state::Screen::Working;
    app.pane = houyicoder_tui::state::Pane::Agents;
    app.agent_directory = None;
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
        color: None,
    });
    let v = app.transcript_version.get();
    app.agents.refresh(&app.transcript, v);
    assert_eq!(
        app.agents.rows.len(),
        1,
        "the returned delegation is listed"
    );
    assert!(app.agents.rows[0].loaded, "loaded fold detected");
    // Enter on the selected row opens that delegation's view.
    houyicoder_tui::keys::handle_working(
        &mut app,
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
    );
    let view = app.teammate_view.as_ref().expect("view opened");
    assert_eq!(view.child_sid, "c1");
    assert!(
        !view.transcript.is_empty(),
        "the loaded fold is copied into the view"
    );
}
/// Arrows walk the returned-delegation list when the fleet is retired, and
/// the cursor clamps at the bounds.
#[test]
fn test_agents_pane_cursor_walks() {
    use houyicoder_tui::records::TranscriptLine;
    let mut app = houyicoder_tui::composition::app();
    app.screen = houyicoder_tui::state::Screen::Working;
    app.pane = houyicoder_tui::state::Pane::Agents;
    for (sid, summary) in [("c1", "first"), ("c2", "second")] {
        app.push_transcript_line(TranscriptLine::Subagent {
            child_sid: sid.into(),
            subagent_type: "explore".into(),
            summary: summary.into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    }
    let v = app.transcript_version.get();
    app.agents.refresh(&app.transcript, v);
    app.agents.move_selection(1);
    assert_eq!(app.agents.sel, 1);
    app.agents.move_selection(1);
    assert_eq!(app.agents.sel, 1, "clamped at the last row");
    app.agents.move_selection(-1);
    assert_eq!(app.agents.sel, 0, "clamped at the first row");
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
