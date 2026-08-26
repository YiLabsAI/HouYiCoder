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
/// shows the inline preview, not raw {"block_ref":"..."} JSON. Ctrl+O
/// expands without leaking the raw marker key. Mutation: disabling
/// field-level in isolate turns this red (the fold-group never appears).
#[test]
#[ignore]
fn test_multi_large_child_summary() {
    let dense = "line \"q\" \\ backslash\nnext line\n".repeat(300);
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
    // The fold-group summary line ("⎿ explore:") appears after the child
    // completes — distinct from the running chip ("Agent(→ explore)").
    // agentId survived field-level externalization.
    assert!(
        s.wait_for_plain("explore:", RENDER_TIMEOUT * 5),
        "Subagent fold-group renders with large child summary:\n{}",
        s.output()
    );
    // Ctrl+O expands: the raw block_ref marker key must not leak.
    s.send_key(&Key::Ctrl('o'));
    assert!(
        !s.output_plain().contains("block_ref"),
        "no raw block_ref key in the expanded view:\n{}",
        s.output()
    );
}
