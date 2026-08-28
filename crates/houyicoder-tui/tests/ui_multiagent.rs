//! Real-binary PTY tests for the multi-agent sync delegation UX.
//! Each test spawns the houyi binary under a PTY with a scripted stub
//! provider, so the full chain runs end-to-end: the parent calls the
//! agent tool, the sync spawn drives the child, the child completes, the
//! parent resumes, and the Subagent fold-group + teammate view render
//! through the real interaction layer. Slow, so ignored by default.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, session_on_working_with_script};

/// PTY real-binary sync delegation full chain. Send a message, the stub
/// returns an agent-tool call, the sync spawn runs the child, the parent
/// resumes, the Subagent fold-group appears, Enter opens the teammate
/// view banner, Esc returns to the parent transcript.
#[test]
#[ignore]
fn test_multi_sync_delegation() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find the auth module","description":"find auth"}}],
        [{"type":"Text","text":"auth is in src/auth"}],
        [{"type":"Text","text":"the auth module is in src/auth"}]
    ]"#;
    let mut s = session_on_working_with_script(script);
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "working screen should render"
    );
    s.send_str("find the auth module");
    s.send_str("\r");
    assert!(
        s.wait_for_plain("explore", RENDER_TIMEOUT * 2),
        "Subagent fold-group should appear after delegation:\n{}",
        s.output()
    );
    assert!(
        s.output_plain().contains("ctrl+o"),
        "expand hint should render:\n{}",
        s.output()
    );
    // Enter opens the teammate view on the last Subagent.
    s.send_str("\r");
    assert!(
        s.wait_for_plain("Viewing", RENDER_TIMEOUT),
        "teammate view banner should render after Enter:\n{}",
        s.output()
    );
    assert!(
        s.output_plain().contains("@explore"),
        "banner should name the viewed agent:\n{}",
        s.output()
    );
    assert!(
        s.output_plain().contains("esc"),
        "banner should carry the esc-return hint:\n{}",
        s.output()
    );
    // Esc exits back to the parent transcript. The working-screen
    // placeholder returning confirms the banner is gone.
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "after Esc, should return to parent transcript:\n{}",
        s.output()
    );
}

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
    let mut s = session_on_working_with_script(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(
        s.wait_for_plain("explore", RENDER_TIMEOUT * 2),
        "Subagent fold should appear"
    );
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for_plain("src/auth", RENDER_TIMEOUT),
        "expanded fold should show child text:\n{}",
        s.output()
    );
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for_plain("ctrl+o", RENDER_TIMEOUT),
        "fold should collapse back"
    );
    s.send_str("\r");
    assert!(
        s.wait_for_plain("Viewing", RENDER_TIMEOUT),
        "teammate view should open after Enter:\n{}",
        s.output()
    );
    s.send_key(&Key::Esc);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
}

/// Large child summary (>8KB, newline + quote dense) — the shape that
/// broke the first B2 fix. Field-level externalization keeps agentId at
/// the top level so the Subagent fold-group still renders; the summary
/// shows clean text (not JSON-escaped), truncated to a one-liner. Ctrl+O
/// expands without leaking the raw marker key. Mutation: disabling
/// field-level in isolate turns this red (the fold-group never appears).
#[test]
#[ignore]
fn test_multi_large_child_summary() {
    let dense = "First sentence of the child analysis.\nSecond line with \"quotes\".\nThird line with \\ backslash.\n".repeat(80);
    let script = serde_json::json!([
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text": dense}],
        [{"type":"Text","text":"done"}]
    ])
    .to_string();
    let mut s = session_on_working_with_script(&script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    // The fold-group summary line appears after the child completes.
    // "explore:" is the fold-group label (distinct from the chip's
    // "Agent(→ explore)"). agentId survived field-level externalization.
    assert!(
        s.wait_for_plain("explore:", RENDER_TIMEOUT * 5),
        "Subagent fold-group renders with large child summary:\n{}",
        s.output()
    );
    // The summary must be clean text, not JSON-escaped: no literal \n,
    // no escaped quotes. The summary is a short one-liner (first ~80
    // chars, newlines flattened to spaces).
    let plain = s.output_plain();
    assert!(
        !plain.contains("\\n"),
        "summary must not show literal backslash-n (JSON-escaped): {plain}"
    );
    assert!(
        !plain.contains("\\\""),
        "summary must not show escaped quotes: {plain}"
    );
    assert!(
        plain.contains("First sentence"),
        "summary shows the child's content text: {plain}"
    );
    // Ctrl+O expands: no raw block_ref key leaks into the expanded view.
    s.send_key(&Key::Ctrl('o'));
    assert!(
        !s.output_plain().contains("block_ref"),
        "no raw block_ref key in the expanded view:\n{}",
        s.output()
    );
}

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
    let mut s = session_on_working_with_script(script);
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
