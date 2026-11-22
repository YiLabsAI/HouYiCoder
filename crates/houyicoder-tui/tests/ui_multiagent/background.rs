use super::*;

/// Real-binary background delegation: the parent delegates a background
/// child (run_in_background), the tool returns a launched result
/// immediately, the parent continues, the detached task runs the child
/// to completion, the notification injector enqueues, and the parent's
/// next run drains the notification at its first turn boundary. The
/// script is all "ok" past turn 1 so the shared provider race (parent vs
/// child consuming turns) cannot break either side. Slow, ignored by
/// default.
#[test]
#[ignore]
fn test_background_delegation_completes() {
    // Turn 1: the agent tool call with run_in_background, then the parent
    // continues with a text. Every later turn is "ok" so the child + the
    // parent's later turns all resolve to a final text regardless of who
    // consumes which scripted turn (the shared provider race).
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth","run_in_background":true}},{"type":"Text","text":"delegated background, continuing"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "working screen renders"
    );
    s.send_str("find the auth module");
    s.send_str("\r");
    // Turn 1: the background spawn fires + the parent continues. "delegated
    // background" confirms the parent did not block on the child.
    assert!(
        s.wait_for_plain("delegated background", RENDER_TIMEOUT * 2),
        "parent should continue past a background delegation:\n{}",
        s.output()
    );
    assert!(
        s.wait_for_screen("Agent \"find auth\" finished", RENDER_TIMEOUT * 2),
        "background child completion should leave the one-row trace in the parent transcript:\n{}",
        s.screen().contents()
    );
    assert!(
        !s.screen().contents().contains("Subagent explore"),
        "child report must not render as a user-authored line:\n{}",
        s.screen().contents()
    );
}

/// The parent continues past a background spawn without blocking: the
/// parent's own text renders right after the launched result, before the
/// child completes. Proves the background path returns immediately (no
/// foreground block).
#[test]
#[ignore]
fn test_background_spawn_unblocks_parent() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth","run_in_background":true}},{"type":"Text","text":"parent carried on"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("parentcarriedon", RENDER_TIMEOUT * 2),
        "parent should continue past a background spawn:\n{}",
        s.output()
    );
}

/// The background completion notification wakes the parent without pasting the
/// child report into the parent transcript, while Enter on the finished trace
/// opens the child transcript holding what the child produced.
#[test]
#[ignore]
fn test_background_output_reaches_parent() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth","run_in_background":true}},{"type":"Text","text":"parent continues"}],
        [{"type":"Text","text":"background child produced this"}],
        [{"type":"Text","text":"parent synthesized answer"}],
        [{"type":"Text","text":"ok"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("parentcontinues", RENDER_TIMEOUT * 2));
    assert!(
        s.wait_for_screen("parent synthesized answer", RENDER_TIMEOUT * 2),
        "parent should wake on the drained completion notice and reply:\n{}",
        s.screen().contents()
    );
    assert!(
        !s.screen()
            .contents()
            .contains("background child produced this"),
        "raw child body stays out of the parent transcript:\n{}",
        s.screen().contents()
    );
    s.send_str("\r");
    assert!(
        s.wait_for_screen("background child produced this", RENDER_TIMEOUT * 2),
        "drilling into the finished trace reveals the child output:\n{}",
        s.screen().contents()
    );
}

// ---- batch 5: agents pane + slash + misc ----

/// The background spawn result surfaces a "launched in the background"
/// message, so the user knows the child is detached (not blocking).
#[test]
#[ignore]
fn test_background_spawn_reports_start() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth","run_in_background":true}},{"type":"Text","text":"parent continues"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("background", RENDER_TIMEOUT * 2),
        "background spawn should surface a background-launch message:\n{}",
        s.output()
    );
}

/// The background completion notification drains into the parent transcript
/// at a turn boundary, proving the detached-child completion reaches the
/// parent. (Counting exact drain events is a unit-level concern; here the
/// render stays across frames so the buffer count is not a drain count.)
#[test]
#[ignore]
fn test_background_notice_arrives_once() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth","run_in_background":true}},{"type":"Text","text":"parent continues"}],
        [{"type":"Text","text":"child finished"}],
        [{"type":"Text","text":"parent noticed ending"}],
        [{"type":"Text","text":"follow up reply"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("parentcontinues", RENDER_TIMEOUT * 2));
    assert!(
        s.wait_for_screen("parent noticed ending", RENDER_TIMEOUT * 2),
        "completion notice should wake the parent once:\n{}",
        s.screen().contents()
    );
    s.send_str("update");
    s.send_str("\r");
    assert!(
        s.wait_for_screen("follow up reply", RENDER_TIMEOUT * 2),
        "later prompt runs cleanly:\n{}",
        s.screen().contents()
    );
    let traces = s
        .screen()
        .contents()
        .matches("Agent \"find auth\" finished")
        .count();
    assert_eq!(
        traces,
        1,
        "finished trace appears once on screen:\n{}",
        s.screen().contents()
    );
}

/// A foreground delegation that outlives the configured wait budget releases
/// the parent's turn while the child is still streaming. The parent's next
/// assistant text renders before the child's own text finishes, so a user can
/// keep working instead of waiting out a long child.
#[test]
#[ignore]
fn test_budget_frees_turn() {
    // The child's text streams in four-character chunks with a delay each, so
    // it stays in flight well past the budget. The parent's shorter text lands
    // inside the child's streaming window, and the sentinel at the child's tail
    // has not arrived yet when the parent text renders. The screen is read
    // rather than the byte stream: the child's redraws interleave with the
    // parent's line in the stream, which a contiguous match cannot survive.
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"long task","description":"long task"}}],
        [{"type":"Text","text":"child still working child still working child still working child still working child still working child still working child still working child still working child still working child still working child still working child still working childstillhere"}],
        [{"type":"Text","text":"parent carried on"}]
    ]"#;
    let env = [
        (
            "HOUYICODER_DELEGATION_FOREGROUND_BUDGET_MS".to_string(),
            "300".to_string(),
        ),
        (
            "HOUYICODER_TEST_STUB_DELAY_MS".to_string(),
            "100".to_string(),
        ),
    ];
    let mut s = pty_session_scripted_env(script, &env);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("delegate the long task");
    s.send_str("\r");
    assert!(
        s.wait_for_screen("parent carried on", RENDER_TIMEOUT * 2),
        "the parent's next turn renders after the budget releases it:\n{}",
        s.output()
    );
    assert!(
        !s.screen().contents().contains("childstillhere"),
        "the parent was released while the child was still streaming:\n{}",
        s.output()
    );
}
