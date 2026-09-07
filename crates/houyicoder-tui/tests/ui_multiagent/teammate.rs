use super::*;

/// Boundary: Ctrl+O expands the inline fold, then Enter opens the teammate
/// view on the same delegation. The two paths (inline expand + teammate
/// view) coexist on the same Subagent line without conflict.
#[test]
#[ignore]
fn test_multi_expand_teammate() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth is in src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_slow_scripted(80, script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    // Wait for the fold-group hint, not "explore" (the footer pill renders
    // "explore: ..." at spawn + would match before the child completes).
    assert!(
        s.wait_for_plain("ctrl+o", RENDER_TIMEOUT * 2),
        "Subagent fold should appear"
    );
    // While the child is live the status bar names it and the keys that act
    // on it: without this the strip is the one surface whose affordance is
    // never stated anywhere, and the selection reads as nonexistent.
    assert!(
        s.wait_for_compact("1agent", RENDER_TIMEOUT * 2),
        "the status bar should count the live agent:\n{}",
        s.output()
    );
    // Settle the run before toggling. While the parent is still streaming,
    // every arriving chunk repaints the transcript, so an expand that failed
    // to invalidate the row cache still appeared on the next chunk and the
    // toggle looked fine. Idle is where a broken toggle is visible.
    assert!(
        s.wait_for_plain("done", RENDER_TIMEOUT * 2),
        "the parent run should finish before the toggle:\n{}",
        s.output()
    );
    // Assert on the expanded BODY, not on the summary. The summary is the
    // child's answer, so it is on screen collapsed too, and matching it
    // proves nothing about the toggle. Clearing first makes the match
    // evidence of this repaint rather than of an earlier one -- the stream
    // accumulates, and ratatui repaints only the cells that changed, so the
    // head's unchanged prefix is not re-emitted and only the flipped tail of
    // the hint arrives.
    s.clear_output();
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for_compact("childtranscript", RENDER_TIMEOUT),
        "expanding should open the delegation's body:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for_compact("expand)", RENDER_TIMEOUT),
        "the head should offer expand again after collapsing:\n{}",
        s.output()
    );
    assert!(
        !s.output_compact().contains("childtranscript"),
        "collapsing should drop the body it opened:\n{}",
        s.output()
    );
    s.send_str("\r");
    assert!(
        s.wait_for_compact("Viewing@explore", RENDER_TIMEOUT),
        "teammate view should open after Enter:\n{}",
        s.output()
    );
    // Shift+Down returns to the parent flow (Esc only interrupts the viewed
    // child's turn; it never exits). Assert the parent's delegation row is
    // repainted and the banner is gone; the input row is identical in both
    // views, so a marker from it is never re-emitted and would only ever
    // match bytes from before the view opened.
    s.clear_output();
    s.send_key(&Key::ShiftDown);
    assert!(
        s.wait_for_compact("explore:authisin", RENDER_TIMEOUT),
        "Shift+Down should repaint the parent transcript:\n{}",
        s.output()
    );
    assert!(
        !s.output_compact().contains("Viewing@explore"),
        "the teammate banner should be gone:\n{}",
        s.output()
    );
}

/// A slash typed inside the teammate view routes to the parent command
/// palette, not the child's inbox: typing "/" while viewing a child opens
/// the parent slash palette (the command list), proving the slash did not
/// route as a steering message to the child. Esc closes the palette, then
/// Shift+Down exits the teammate view (Esc only interrupts the child).
/// Slow, ignored by default.
#[test]
#[ignore]
fn test_teammate_slash_routes_parent() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth is in src/auth"}],
        [{"type":"Text","text":"the auth module is in src/auth"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_plain("ctrl+o", RENDER_TIMEOUT * 2));
    s.send_str("\r");
    assert!(
        s.wait_for_plain("Viewing", RENDER_TIMEOUT),
        "teammate view should open:\n{}",
        s.output()
    );
    // Type a slash while viewing the child. The slash opens the parent
    // command palette — it does not route to the child inbox (which would
    // silently consume the text with no palette). The palette header is
    // the proof the slash reached the parent command path.
    s.send_key(&Key::Char('/'));
    assert!(
        s.wait_for_plain("commands", RENDER_TIMEOUT),
        "slash should open the parent command palette, not route to the \
         child:\n{}",
        s.output()
    );
    // Esc closes the palette; Shift+Down exits the teammate view (Esc only
    // interrupts the viewed child's turn, it never exits).
    s.send_key(&Key::Esc);
    s.send_key(&Key::ShiftDown);
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "Esc closes the palette, Shift+Down exits the teammate view:\n{}",
        s.output()
    );
}

/// Steering a completed child exits the teammate view + surfaces a
/// "finished" notice in the parent transcript (visible at the tail), so the
/// user learns the child is done + is back at the parent to start a new
/// task. The unit test (test_steer_completed_surfaces_notice) checks the
/// state; this is the real-binary end-to-end. Slow, ignored by default.
#[test]
#[ignore]
fn test_steer_completed_notice() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth is in src/auth"}],
        [{"type":"Text","text":"the auth module is in src/auth"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_plain("ctrl+o", RENDER_TIMEOUT * 2));
    s.send_str("\r");
    assert!(s.wait_for_plain("Viewing", RENDER_TIMEOUT));
    s.send_str("do more analysis");
    s.send_str("\r");
    assert!(
        s.wait_for_plain("has finished", RENDER_TIMEOUT),
        "steering a completed child should surface the finished notice:\n{}",
        s.output()
    );
}

// ---- batch 1: sync delegation + fold-group interaction ----

/// Enter the teammate view, Esc out, then re-Enter on the same fold. Proves
/// the drill-in is idempotent (re-entry does not get stuck or refuse).
#[test]
#[ignore]
fn test_sync_reenter_teammate() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth in src/auth"}],
        [{"type":"Text","text":"parent resumed"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    s.send_str("\r");
    assert!(s.wait_for_plain("Viewing", RENDER_TIMEOUT));
    s.send_key(&Key::Esc);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("\r");
    assert!(
        s.wait_for_plain("Viewing", RENDER_TIMEOUT),
        "re-entering the teammate view should work:\n{}",
        s.output()
    );
}

/// Entering the teammate view shows the child's transcript body (the child's
/// returned text), not just the banner. Proves the drill-in surfaces child
/// content, the keyboard path to see what the child produced.
#[test]
#[ignore]
fn test_teammate_view_child_content() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"the auth boundary is src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    s.send_str("\r");
    assert!(s.wait_for_plain("Viewing", RENDER_TIMEOUT));
    assert!(
        s.output_compact().contains("authboundaryissrc/auth"),
        "teammate view body should show the child text:\n{}",
        s.output()
    );
}

/// The teammate-view banner carries the task prompt on its second line, so
/// the user knows what the viewed child was asked to do.
#[test]
#[ignore]
fn test_teammate_prompt_surfaces() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"locate the auth boundary","description":"find auth"}}],
        [{"type":"Text","text":"auth in src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    s.send_str("\r");
    assert!(s.wait_for_plain("Viewing", RENDER_TIMEOUT));
    assert!(
        s.output_compact().contains("locatetheauthboundary"),
        "banner should carry the task prompt:\n{}",
        s.output()
    );
}

/// A non-explore subagent type renders its own label in the banner, proving
/// the type is not hard-coded to one value. Uses the registered "plan" type.
#[test]
#[ignore]
fn test_teammate_banner_plan() {
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
    s.send_str("\r");
    assert!(s.wait_for_plain("Viewing", RENDER_TIMEOUT));
    assert!(
        s.output_compact().contains("@plan"),
        "banner should name the plan type:\n{}",
        s.output()
    );
}

// ---- batch 2: Esc interrupt + recall (the two-press model) ----

/// The teammate-view banner carries the shift-arrow return hint, so the
/// user knows how to exit the view without guessing.
#[test]
#[ignore]
fn test_teammate_banner_hint() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth in src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    s.send_str("\r");
    assert!(s.wait_for_plain("Viewing", RENDER_TIMEOUT));
    assert!(
        s.output_compact().contains("shift+↑↓return"),
        "banner should carry the shift-arrow return hint:\n{}",
        s.output()
    );
}

/// Typing printable chars inside the teammate view routes to the parent
/// input (the chars echo in the parent input box), not to the child inbox.
#[test]
#[ignore]
fn test_teammate_chars_route_parent() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth in src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    s.send_str("\r");
    assert!(s.wait_for_plain("Viewing", RENDER_TIMEOUT));
    s.send_str("parenttext");
    assert!(
        s.wait_for_compact("parenttext", RENDER_TIMEOUT),
        "typed chars should route to the parent input:\n{}",
        s.output()
    );
}
