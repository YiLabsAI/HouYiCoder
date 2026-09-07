use super::*;

/// Teammate-view pill + stay journey: when the user drills into a
/// completed child's transcript, the footer pill renders alongside (the
/// row the user is reading), and the view stays on normal completion — the
/// user exits with Esc, not an auto-dismiss. The pill-pin past the grace
/// window (the row does not retire while the child is being viewed) is
/// pinned at the unit level (test_retire_pins_viewed_child); this journey
/// covers the end-to-end rendering + the stay-on-complete contract. Slow,
/// ignored by default.
#[test]
#[ignore]
fn test_teammate_pill_pins_view() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth is in src/auth"}],
        [{"type":"Text","text":"the auth module is in src/auth"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    // The fold-group hint lands once the child completes — not the running
    // pill text, which renders at spawn and would match before completion.
    assert!(
        s.wait_for_plain("ctrl+o", RENDER_TIMEOUT * 2),
        "Subagent fold-group should appear after delegation:\n{}",
        s.output()
    );
    // Enter the teammate view on the just-completed child. The first full
    // render of the view carries the banner + the pill row (the child's
    // terse done row), so the token marker is present alongside the banner.
    s.send_str("\r");
    assert!(
        s.wait_for_plain("Viewing", RENDER_TIMEOUT),
        "teammate view banner should render:\n{}",
        s.output()
    );
    assert!(
        s.output_plain().contains("tok"),
        "pill should render alongside the viewed child's transcript:\n{}",
        s.output()
    );
    // The view stays on normal completion — no auto-dismiss fires for a
    // completed (non-killed, non-failed) child. Wait past the grace window
    // to prove the stay is not a transient render: the banner is still the
    // active state (Shift+Down exits; Esc is a no-op on a completed child).
    std::thread::sleep(FLEET_GRACE + Duration::from_secs(2));
    // clear the buffer so the post-exit frame is what we assert on, not the
    // pre-exit banner bytes still in the scrollback.
    s.clear_output();
    s.send_key(&Key::ShiftDown);
    assert!(
        s.wait_for_compact("explore:authisinsrc/auth", RENDER_TIMEOUT),
        "after Shift+Down the parent delegation fold-group should repaint:\n{}",
        s.output()
    );
    assert!(
        !s.output_compact().contains("Viewing@explore"),
        "Shift+Down should exit the teammate view (banner gone), the view \
         stayed until Shift+Down not auto-dismissed on completion:\n{}",
        s.output()
    );
}

/// After a sync delegation completes, the footer pill renders the child's
/// done row (the type + a done marker + the token total). Proves the pill
/// surfaces the terminal state, not just the running state.
#[test]
#[ignore]
fn test_pill_done_after_completion() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth in src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2),
        "fold should render:\n{}",
        s.output()
    );
    assert!(
        s.output_compact().contains("explore·done"),
        "pill should show the done row after completion:\n{}",
        s.output()
    );
}

/// Two sync delegations in one run leave two footer pill rows, each
/// carrying its own type + done marker. Proves the pill tracks multiple
/// children (not last-writer-wins) and each row is typed by its delegation.
#[test]
#[ignore]
fn test_pill_two_children_rows() {
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
    assert!(
        s.wait_for_compact("parentdone", RENDER_TIMEOUT * 3),
        "both children should complete:\n{}",
        s.output()
    );
    let pc = s.output_compact();
    assert!(
        pc.contains("explore·done") && pc.contains("plan·done"),
        "both child pill rows should render with their own type:\n{}",
        s.output()
    );
}

/// While a sync child is in-flight, the footer pill renders the running row
/// (the type + a live verb), distinct from the done row. Proves the pill
/// tracks the running state before completion. Uses the stub delay so the
/// in-flight window is wide enough to catch.
#[test]
#[ignore]
fn test_pill_running_verb_inflight() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth in src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = common::pty_session_slow_scripted(2000, script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    // While the child is in-flight (2s delay), the pill shows the running
    // row (type + verb, colon-separated) before the done row replaces it.
    assert!(
        s.wait_for_compact("explore:", RENDER_TIMEOUT * 2),
        "pill should render the running row while the child is in-flight:\n{}",
        s.output()
    );
    assert!(
        !s.output_compact().contains("explore·done"),
        "pill should not show done before the child completes:\n{}",
        s.output()
    );
}

/// Shift+Down on the footer pill moves the selection onto a child row; Enter
/// then opens that child's teammate view. Proves the fleet-selection drill-in
/// path (distinct from the transcript-line Enter path).
#[test]
#[ignore]
fn test_pill_shift_enter_teammate() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth in src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("explore·done", RENDER_TIMEOUT * 2));
    s.send_key(&Key::ShiftDown);
    s.send_str("\r");
    assert!(
        s.wait_for_plain("Viewing", RENDER_TIMEOUT),
        "Enter on the fleet selection should open the teammate view:\n{}",
        s.output()
    );
}

// ---- batch 4: async delegation ----

/// The running pill shows the live-progress glyph (a hollow circle),
/// distinct from the done row's check mark. Proves the pill distinguishes
/// in-flight from completed at the glyph level.
#[test]
#[ignore]
fn test_pill_running_glyph() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth in src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = common::pty_session_slow_scripted(2000, script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("◯", RENDER_TIMEOUT * 2),
        "running pill should show the hollow-circle glyph:\n{}",
        s.output()
    );
}

/// The pill transitions from the running row to the done row as a sync child
/// completes: the running verb appears first, then the done marker replaces
/// it. Proves the pill reflects the live state change at completion.
#[test]
#[ignore]
fn test_pill_running_to_done() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth in src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = common::pty_session_slow_scripted(2000, script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    // Running row first (colon-separated type + verb), then the done row.
    assert!(s.wait_for_compact("explore:", RENDER_TIMEOUT * 2));
    assert!(
        s.wait_for_compact("explore·done", RENDER_TIMEOUT * 3),
        "pill should transition to the done row after completion:\n{}",
        s.output()
    );
}

// ---- batch 6: banner, palette, edge, unicode ----

/// The completed pill row shows the check-mark glyph, distinct from the
/// running row's hollow circle. Proves the terminal-state glyph lands.
#[test]
#[ignore]
fn test_pill_completed_check_glyph() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth in src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("✓explore", RENDER_TIMEOUT * 2),
        "completed pill should show the check glyph + type:\n{}",
        s.output()
    );
}

/// Shift+Up/Down on the footer pill moves the selection without crashing;
/// Enter on the selection opens the teammate view. Proves the fleet
/// selection keys are wired both directions.
#[test]
#[ignore]
fn test_pill_shift_arrows_enter() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth in src/auth"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("✓explore", RENDER_TIMEOUT * 2));
    s.send_key(&Key::ShiftUp);
    s.send_key(&Key::ShiftDown);
    s.send_str("\r");
    assert!(
        s.wait_for_plain("Viewing", RENDER_TIMEOUT),
        "Enter on the fleet selection should open the teammate view:\n{}",
        s.output()
    );
}

/// Enter a teammate view on a RUNNING child via the footer pill (Shift+Down
/// selects the pill row, empty-input Enter drills). This is the
/// click-pill-on-running-child path that the cache-stale bug hit: a running
/// child has no fold anchor row, so the view opens empty and fills on fetch.
/// The unit test pins the fill-bump logic; this journey pins the real-binary
/// key path + that the parent transcript does not bleed through. Slow,
/// ignored by default.
#[test]
#[ignore]
fn test_pill_enter_running_child() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth","run_in_background":true}},{"type":"Text","text":"delegated async, continuing"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find the auth module");
    s.send_str("\r");
    assert!(
        s.wait_for_plain("delegated async", RENDER_TIMEOUT * 2),
        "parent should continue past an async delegation:\n{}",
        s.output()
    );
    // Clear so the post-drill frame is what we assert on, then select the
    // pill + drill.
    s.clear_output();
    s.send_key(&Key::ShiftDown);
    s.send_str("\r");
    assert!(
        s.wait_for_plain("Viewing", RENDER_TIMEOUT),
        "pill drill should open the teammate view on the running child:\n{}",
        s.output()
    );
    assert!(
        !s.output_plain().contains("delegated async"),
        "parent transcript must not render inside the child view:\n{}",
        s.output()
    );
}
