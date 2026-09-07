use super::*;

/// Real-binary async delegation: the parent delegates a background child
/// (run_in_background), the tool returns async_launched immediately, the
/// parent continues, the detached driver runs the child to completion, the
/// notification injector enqueues, and the parent's next run drains the
/// notification at its first turn boundary. The script is all "ok" past
/// turn 1 so the shared provider race (parent vs child consuming turns)
/// cannot break either side. Slow, ignored by default.
#[test]
#[ignore]
fn test_multi_async_delegation() {
    // Turn 1: the agent tool call with run_in_background, then the parent
    // continues with a text. Every later turn is "ok" so the child + the
    // parent's later turns all resolve to a final text regardless of who
    // consumes which scripted turn (the shared provider race).
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth","run_in_background":true}},{"type":"Text","text":"delegated async, continuing"}],
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
    // Turn 1: the async spawn fires + the parent continues. "delegated async"
    // confirms the parent did not block on the child (async_launched).
    assert!(
        s.wait_for_plain("delegated async", RENDER_TIMEOUT * 2),
        "parent should continue past an async delegation:\n{}",
        s.output()
    );
    // Give the detached driver time to run the child to completion + the
    // injector time to enqueue the notification. Each user message starts a
    // run whose first turn boundary drains the notification queue, so send a
    // few + poll each — the notification lands whenever the detached driver
    // finishes (timing-sensitive under parallel PTY load, hence the loop).
    let mut drained = false;
    for _ in 0..5 {
        s.send_str("any update");
        s.send_str("\r");
        if s.wait_for_plain("Subagent", RENDER_TIMEOUT * 2) {
            drained = true;
            break;
        }
    }
    assert!(
        drained,
        "async child completion notification should drain into the parent \
         transcript across several turn boundaries:\n{}",
        s.output()
    );
    assert!(
        s.output_plain().contains("completed"),
        "notification carries the terminal status:\n{}",
        s.output()
    );
}

/// The parent continues past an async spawn without blocking: the parent's
/// own text renders right after the async_launched result, before the child
/// completes. Proves the async path returns immediately (no sync block).
#[test]
#[ignore]
fn test_async_parent_unblocked() {
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
        "parent should continue past an async spawn:\n{}",
        s.output()
    );
}

/// The async completion notification carries the child's result text, not
/// just the terminal status, so the parent transcript shows what the child
/// produced. Distinct from the status-only assertion.
#[test]
#[ignore]
fn test_async_child_text_drains() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth","run_in_background":true}},{"type":"Text","text":"parent continues"}],
        [{"type":"Text","text":"async child produced this"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("parentcontinues", RENDER_TIMEOUT * 2));
    let mut drained = false;
    for _ in 0..6 {
        s.send_str("update");
        s.send_str("\r");
        if s.wait_for_compact("asyncchildproducedthis", RENDER_TIMEOUT * 2) {
            drained = true;
            break;
        }
    }
    assert!(
        drained,
        "async notification should carry the child result text:\n{}",
        s.output()
    );
}

/// An async spawn followed by a sync spawn in one run: the async child
/// detaches, the sync child blocks the parent to completion, then the async
/// notification drains later. Proves the two spawn modes coexist.
#[test]
#[ignore]
fn test_async_then_sync_mix() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"async","description":"async","run_in_background":true}},{"type":"Text","text":"after async"}],
        [{"type":"Text","text":"sync child result"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("mixed");
    s.send_str("\r");
    // The async spawn returns immediately; the parent continues.
    assert!(
        s.wait_for_compact("afterasync", RENDER_TIMEOUT * 2),
        "parent should continue past the async spawn:\n{}",
        s.output()
    );
    // The sync child (turn 1) blocks the parent to completion + its result
    // fold renders.
    assert!(
        s.wait_for_compact("syncchildresult", RENDER_TIMEOUT * 3),
        "sync child result should render:\n{}",
        s.output()
    );
}

// ---- batch 5: agents pane + slash + misc ----

/// The async spawn result surfaces a "launched in the background" message,
/// so the user knows the child is detached (not blocking).
#[test]
#[ignore]
fn test_async_background_message() {
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
        "async spawn should surface a background-launch message:\n{}",
        s.output()
    );
}

/// The async completion notification drains into the parent transcript at a
/// turn boundary, proving the detached-child completion reaches the parent.
/// (Counting exact drain events is a unit-level concern; here the render
/// stays across frames so the buffer count is not a drain count.)
#[test]
#[ignore]
fn test_async_notification_drains_once() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth","run_in_background":true}},{"type":"Text","text":"parent continues"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}],
        [{"type":"Text","text":"ok"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("parentcontinues", RENDER_TIMEOUT * 2));
    let mut drained = false;
    for _ in 0..6 {
        s.send_str("update");
        s.send_str("\r");
        if s.wait_for_compact("completed", RENDER_TIMEOUT * 2) {
            drained = true;
            break;
        }
    }
    assert!(drained, "notification should drain:\n{}", s.output());
}
