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

// ---- batch 1: foreground delegation + fold-group interaction ----

/// Enter the teammate view, Esc out, then re-Enter on the same fold. Proves
/// the drill-in is idempotent (re-entry does not get stuck or refuse).
#[test]
#[ignore]
fn test_foreground_reenter_teammate() {
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

// ---- batch 2: Esc interrupt and teammate controls ----

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

/// A page parked mid-history keeps its rows on screen across a visit into the
/// teammate view: entering the child must not reset the parent scroll and
/// leaving must not force it back to the tail. The reply is long enough that
/// its head row is out of the tail window, so the marker only renders while
/// the park holds.
#[test]
#[ignore]
fn test_parent_park_survives_child() {
    // One script message per model call: the parent opens the delegation, the
    // child answers, then the parent's single reply is long enough that PageUp
    // has real rows to park above the tail window.
    let fill = "lorem ipsum dolor sit amet consectetur adipiscing elit ".repeat(200);
    let script = format!(
        r#"[[{{"type":"ToolCall","id":"toolu_1","name":"agent","input":{{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}}}],[{{"type":"Text","text":"child done"}}],[{{"type":"Text","text":"PARK ROW EARLY {fill} END SENTINEL"}}]]"#
    );
    let mut s = pty_session_scripted(&script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("ENDSENTINEL", RENDER_TIMEOUT * 2),
        "the turn finished streaming:\n{}",
        s.output()
    );
    // The first PageUp only switches to the scroll view (it publishes the
    // scroll cap there); the rest page with that cap until the head row
    // comes into view.
    s.send_key(&Key::PageUp);
    for _ in 0..8 {
        s.send_key(&Key::PageUp);
    }
    assert!(
        s.wait_for_screen("PARK ROW EARLY", RENDER_TIMEOUT),
        "the second PageUp parks the viewport on the head row:\n{}",
        s.screen().contents()
    );
    // Leave scroll mode without paging: a graphic key exits the scroll view
    // back to input, keeping the offset where it stands. The keypress itself
    // is consumed by the exit, so the input box stays empty for Enter to
    // drill into the child.
    s.send_str("x");
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(
        s.screen().contents().contains("PARK ROW EARLY"),
        "leaving the scroll view keeps the parked rows:\n{}",
        s.screen().contents()
    );
    s.send_str("\r");
    assert!(
        s.wait_for_compact("Viewing@explore", RENDER_TIMEOUT),
        "Enter opens the teammate view:\n{}",
        s.output()
    );
    s.send_key(&Key::ShiftDown);
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(
        s.screen().contents().contains("PARK ROW EARLY"),
        "the exit repaints the parent at the parked rows:\n{}",
        s.screen().contents()
    );
}

/// A child-to-child hop carries no expansion state between the two views:
/// the leaving child parks its open blocks under its own id, and the
/// entering child starts from what it parked itself, nothing else. Both
/// children answer with one reasoning turn and one reply, so the two logs
/// hold the same frame shape and their turn rows take the same name — a
/// leak between them would be structural, not a coincidence of content. The
/// re-entry at the end proves the parked expansion comes back without a
/// fresh ctrl+o. Slow, ignored by default.
#[test]
#[ignore]
fn test_hop_expansion_isolation() {
    // One delegation per parent call, in order: the stub answers callers
    // from one shared queue, so two tool calls in a single response would
    // race for the child replies and swap the children's transcripts.
    let script = r##"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"first","description":"first"}}],
        [{"type":"Reasoning","text":"ATHINK weighing the first option"},{"type":"Text","text":"first-child-result"}],
        [{"type":"ToolCall","id":"toolu_2","name":"agent","input":{"subagent_type":"plan","prompt":"second","description":"second"}}],
        [{"type":"Reasoning","text":"BTHINK weighing the second option"},{"type":"Text","text":"second-child-result"}],
        [{"type":"Text","text":"parent done"}]
    ]"##;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("delegate two");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("parentdone", RENDER_TIMEOUT * 3),
        "both delegations complete and the parent resumes:\n{}",
        s.output()
    );
    // A completed footer row drops five seconds after completion, and the
    // agents pane lists the returned delegations only once the rows are
    // gone: while one stands, Enter on the pane follows the footer
    // selection instead, which is empty here.
    std::thread::sleep(Duration::from_secs(6));
    // Drain the parent-phase stream first: the child's result text also
    // rendered while the parent was live, so a fill latch scanned against
    // the undrained stream matches that residue and the expand key goes
    // out before the view's own rows exist.
    s.clear_output();
    // Drill into the newest delegation (the plan child) and expand its
    // reasoning block.
    s.send_str("\r");
    assert!(
        s.wait_for_compact("Viewing@plan", RENDER_TIMEOUT),
        "Enter opens the newest teammate view:\n{}",
        s.output()
    );
    // The view opens empty and fills when the fetch from the child's log
    // lands; after the drain above only the fill can emit this text, so it
    // is the latch that the rows exist for the expand key to act on.
    assert!(
        s.wait_for_compact("second-child-result", RENDER_TIMEOUT * 2),
        "the child's rows fill the view:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for_compact("BTHINK", RENDER_TIMEOUT),
        "ctrl+o expands the viewed child's reasoning:\n{}",
        s.output()
    );
    // Open the agents pane over the standing view and Enter the first
    // delegation: a hop that must park the plan child's expansion and open
    // the explore child with its own state.
    s.send_str("/agents");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_compact("first-child-result", RENDER_TIMEOUT),
        "the pane lists the returned delegations:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_key(&Key::Enter);
    // Esc closes the pane back to the transcript; the hop already
    // happened under it.
    s.send_key(&Key::Esc);
    // The banner's leading cells do not change across the hop, so the
    // stream carries only the redrawn child name; the standing screen
    // holds the full banner.
    assert!(
        s.wait_for_screen("Viewing @explore", RENDER_TIMEOUT),
        "the pane Enter hops to the first child's view:\n{}",
        s.screen().contents()
    );
    assert!(
        s.wait_for_compact("first-child-result", RENDER_TIMEOUT * 2),
        "the hopped-to view fills from the child's log:\n{}",
        s.output()
    );
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        !s.output_compact().contains("ATHINK"),
        "the entered child must not inherit the leaving child's expansion:\n{}",
        s.output()
    );
    // Back to the parent, then re-enter the plan child: its parked
    // expansion returns without a fresh ctrl+o.
    s.clear_output();
    s.send_key(&Key::ShiftDown);
    assert!(
        s.wait_for_compact("parentdone", RENDER_TIMEOUT),
        "Shift+Down repaints the parent transcript:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_str("\r");
    assert!(
        s.wait_for_compact("Viewing@plan", RENDER_TIMEOUT),
        "Enter reopens the newest teammate view:\n{}",
        s.output()
    );
    assert!(
        s.wait_for_compact("BTHINK", RENDER_TIMEOUT),
        "the parked reasoning block is restored on re-entry:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_key(&Key::ShiftDown);
    assert!(
        s.wait_for_compact("parentdone", RENDER_TIMEOUT),
        "the final exit repaints the parent transcript:\n{}",
        s.output()
    );
}

/// A search opened while a child view stands is refused with a toast that
/// names the exit gesture: the search reads the parent session's durable
/// log, so opening it under the child view would count matches against
/// rows the user cannot see and jump the hidden parent's viewport. After
/// the exit the same command opens the search view normally. Slow,
/// ignored by default.
#[test]
#[ignore]
fn test_child_search_refusal() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth is in src/auth"}],
        [{"type":"Text","text":"the needle is in the haystack"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find the needle");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("haystack", RENDER_TIMEOUT * 2),
        "the run finishes before the view opens:\n{}",
        s.output()
    );
    s.send_str("\r");
    assert!(
        s.wait_for_plain("Viewing", RENDER_TIMEOUT),
        "Enter opens the teammate view:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_str("/search needle");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_compact("exittheteammateview", RENDER_TIMEOUT),
        "the refusal toast names the exit gesture:\n{}",
        s.output()
    );
    std::thread::sleep(Duration::from_millis(400));
    let screen = s.screen().contents();
    assert!(
        !screen.contains("SEARCH"),
        "the search view must not open under the child view:\n{screen}"
    );
    assert!(
        screen.contains("Viewing"),
        "the child view stands through the refusal:\n{screen}"
    );
    // The same command opens the search once the view is exited.
    s.clear_output();
    s.send_key(&Key::ShiftDown);
    assert!(
        s.wait_for_compact("haystack", RENDER_TIMEOUT),
        "Shift+Down repaints the parent:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_str("/search needle");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("SEARCH", RENDER_TIMEOUT),
        "the search view opens from the parent view:\n{}",
        s.output()
    );
    s.send_key(&Key::Char('q'));
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "q exits the search view:\n{}",
        s.output()
    );
}
