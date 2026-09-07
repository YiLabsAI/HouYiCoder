use super::*;

/// Esc inside a teammate view does NOT exit it. Esc only interrupts the
/// viewed child's current turn; exit is Shift+Up/Down, never Esc, so a
/// misguessed Esc does not drop the user out of the view. On a completed
/// child Esc is a no-op on the run, so the banner must stay. The old
/// version asserted the empty-input placeholder, rendered in both the
/// parent and the view, a false green that passed whether or not the exit
/// happened. The real assertion is the state-specific banner.
#[test]
#[ignore]
fn test_teammate_esc_keeps_view() {
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
    s.send_key(&Key::Esc);
    // Esc never exits the view; the banner must still render a frame later.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        s.output_plain().contains("Viewing"),
        "Esc must not exit the teammate view (banner should stay):\n{}",
        s.output()
    );
}

/// Esc mid-run interrupts the run: the Interrupted notice lands and the
/// in-flight text never renders (the run aborted before completion). Proves
/// the first Esc is an interrupt, not a recall or a no-op.
#[test]
#[ignore]
fn test_esc_busy_interrupts_run() {
    let script = r#"[[{"type":"Text","text":"should not finish"}]]"#;
    let mut s = common::pty_session_slow_scripted(3000, script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("go");
    s.send_str("\r");
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for_compact("Interrupted", RENDER_TIMEOUT * 2),
        "Esc mid-run should surface the Interrupted notice:\n{}",
        s.output()
    );
    assert!(
        !s.output_compact().contains("shouldnotfinish"),
        "interrupted run should not render the in-flight text:\n{}",
        s.output()
    );
}

/// After Esc interrupts a run, the input box is editable: the user can type
/// a new message right away. Proves the post-interrupt state is clean (not
/// stuck busy, not locked), so the user can redirect immediately.
#[test]
#[ignore]
fn test_esc_interrupt_then_edit() {
    let script = r#"[[{"type":"Text","text":"slow reply"}]]"#;
    let mut s = common::pty_session_slow_scripted(3000, script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("go");
    s.send_str("\r");
    s.send_key(&Key::Esc);
    assert!(s.wait_for_compact("Interrupted", RENDER_TIMEOUT * 2));
    s.clear_output();
    s.send_str("nextmessage");
    assert!(
        s.wait_for_compact("nextmessage", RENDER_TIMEOUT),
        "input should be editable after interrupt:\n{}",
        s.output()
    );
}

/// Esc when idle with an empty queue is a no-op: no panic, no quit, the
/// working screen persists and the app still accepts input.
#[test]
#[ignore]
fn test_esc_idle_noop() {
    let mut s = common::pty_session_scripted(r#"[[{"type":"Text","text":"reply"}]]"#);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_key(&Key::Esc);
    s.send_key(&Key::Esc);
    assert!(
        s.output().contains("let's build"),
        "idle Esc should not quit:\n{}",
        s.output()
    );
    s.send_str("z");
    assert!(
        s.wait_for("z", RENDER_TIMEOUT),
        "app should still accept input after idle Esc:\n{}",
        s.output()
    );
}

// ---- batch 3: fleet pill (footer) ----

/// A slash opens the parent command palette (the command list), proving the
/// palette is reachable from the working screen + lists entries.
#[test]
#[ignore]
fn test_slash_opens_palette() {
    let mut s = common::pty_session_scripted(r#"[[{"type":"Text","text":"reply"}]]"#);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_key(&Key::Char('/'));
    assert!(
        s.wait_for_compact("commands", RENDER_TIMEOUT),
        "slash should open the command palette:\n{}",
        s.output()
    );
    s.send_key(&Key::Esc);
}

/// After Esc exits the teammate view, the fold-group persists in the parent
/// transcript (the delegation result is not lost on view exit).
#[test]
#[ignore]
fn test_teammate_esc_fold_survives() {
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
    s.send_key(&Key::Esc);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    assert!(
        s.output_compact().contains("ctrl+otoexpand"),
        "fold should persist after exiting the teammate view:\n{}",
        s.output()
    );
}

/// Esc closes the slash palette (the command list disappears), returning the
/// user to the working screen without submitting a command.
#[test]
#[ignore]
fn test_slash_esc_closes_palette() {
    let mut s = common::pty_session_scripted(r#"[[{"type":"Text","text":"reply"}]]"#);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_key(&Key::Char('/'));
    assert!(s.wait_for_compact("commands", RENDER_TIMEOUT));
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "Esc should close the palette + return to working:\n{}",
        s.output()
    );
}

/// After Esc interrupts a run, the busy indicator clears (no lingering
/// "Working" spinner). Proves the interrupt fully resets the busy state.
#[test]
#[ignore]
fn test_esc_interrupt_clears_busy() {
    let script = r#"[[{"type":"Text","text":"slow reply"}]]"#;
    let mut s = common::pty_session_slow_scripted(3000, script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("go");
    s.send_str("\r");
    assert!(s.wait_for_compact("Working", RENDER_TIMEOUT * 2));
    s.send_key(&Key::Esc);
    assert!(s.wait_for_compact("Interrupted", RENDER_TIMEOUT * 2));
    s.clear_output();
    s.send_str(" ");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !s.output_compact().contains("Working"),
        "busy indicator should clear after interrupt:\n{}",
        s.output()
    );
}
