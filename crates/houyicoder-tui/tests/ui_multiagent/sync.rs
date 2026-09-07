use super::*;

/// PTY real-binary sync delegation full chain. Send a message, the stub
/// returns an agent-tool call, the sync spawn runs the child, the parent
/// resumes, the Subagent fold-group appears, Enter opens the teammate
/// view banner, Shift+Down returns to the parent transcript (Esc only
/// interrupts the viewed child's turn).
#[test]
#[ignore]
fn test_multi_sync_delegation() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find the auth module","description":"find auth"}}],
        [{"type":"Text","text":"auth is in src/auth"}],
        [{"type":"Text","text":"the auth module is in src/auth"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "working screen should render"
    );
    s.send_str("find the auth module");
    s.send_str("\r");
    // Wait for the fold-group expand hint, which only renders once the child
    // completes — NOT "explore"/"explore:", which the footer pill renders at
    // spawn ("explore: thinking") and would match prematurely.
    assert!(
        s.wait_for_plain("ctrl+o", RENDER_TIMEOUT * 2),
        "Subagent fold-group should appear after delegation:\n{}",
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
        s.output_plain().contains("shift"),
        "banner should carry the shift-arrow return hint:\n{}",
        s.output()
    );
    // Shift+Down exits back to the parent transcript (Esc only interrupts
    // the viewed child's turn). The empty-input placeholder is identical in
    // both views, so assert the state-specific banner is gone + the parent's
    // delegation row repaints — not the placeholder.
    s.clear_output();
    s.send_key(&Key::ShiftDown);
    assert!(
        !s.output_plain().contains("Viewing"),
        "after Shift+Down the teammate banner must be gone:\n{}",
        s.output()
    );
    assert!(
        s.wait_for_compact("ctrl+o", RENDER_TIMEOUT),
        "after exit the parent delegation fold-group should repaint:\n{}",
        s.output()
    );
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
    let mut s = pty_session_scripted(&script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    // The fold-group summary line appears after the child completes. Wait
    // for the child's content text, not "explore:" — the footer pill renders
    // "explore: thinking" at spawn + would match before completion. The
    // summary one-liner carries the first ~80 chars of the child text. The
    // check is whitespace-agnostic: under parallel PTY load, ratatui's
    // cell-diff rendering can collapse the spaces between words, so a
    // spaced marker flakes; the compacted form is stable.
    let deadline = std::time::Instant::now() + RENDER_TIMEOUT * 5;
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        let compact: String = s
            .output_plain()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if compact.contains("Firstsentenceofthechildanalysis") {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        ok,
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

/// The child's returned text appears in the collapsed fold-group summary
/// head. Proves the child output reaches the parent transcript (not a silent
/// drop) and the summary line carries real content.
#[test]
#[ignore]
fn test_sync_child_summary_text() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"the auth module lives in src/auth"}],
        [{"type":"Text","text":"parent resumed"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2),
        "fold should render collapsed:\n{}",
        s.output()
    );
    assert!(
        s.output_compact().contains("authmodulelives"),
        "summary head should carry the child text:\n{}",
        s.output()
    );
}

/// The agent-tool call row renders with the subagent type, distinct from
/// the fold-group below it. Proves the delegation call surfaces in the
/// transcript (the ⏺ Agent(→ type) row), not just the result fold.
#[test]
#[ignore]
fn test_sync_agent_call_row() {
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
        s.output_compact().contains("Agent(→explore)"),
        "agent call row should name the subagent type:\n{}",
        s.output()
    );
}

/// After a delegation completes and the parent resumes, a follow-up user
/// message renders normally and the fold-group persists above it. Proves the
/// parent run loop is intact post-delegation.
#[test]
#[ignore]
fn test_sync_followup_persists() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"child found auth"}],
        [{"type":"Text","text":"parent first reply"}],
        [{"type":"Text","text":"parent second reply"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("firstreply", RENDER_TIMEOUT * 2));
    s.send_str("more");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("secondreply", RENDER_TIMEOUT * 2),
        "follow-up should render:\n{}",
        s.output()
    );
    assert!(
        s.output_compact().contains("childfoundauth"),
        "fold should persist after follow-up:\n{}",
        s.output()
    );
}

/// A child that returns empty text does not crash: the fold renders and the
/// parent resumes. Proves the empty-content edge is handled.
#[test]
#[ignore]
fn test_sync_empty_child_safe() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":""}],
        [{"type":"Text","text":"parent resumed"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("parentresumed", RENDER_TIMEOUT * 3),
        "parent should resume after an empty child:\n{}",
        s.output()
    );
    assert!(
        s.output_compact().contains("ctrl+o"),
        "fold should still render for an empty child:\n{}",
        s.output()
    );
}

/// A child whose text spans multiple lines flattens to a one-line summary in
/// the collapsed fold head (newlines become spaces, not literal \n).
#[test]
#[ignore]
fn test_sync_child_multiline_summary() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"line one\nline two\nline three"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    let pc = s.output_compact();
    assert!(
        pc.contains("lineone") && pc.contains("linetwo"),
        "summary should flatten newlines into one line:\n{}",
        s.output()
    );
    assert!(
        !pc.contains("\\n"),
        "summary should not show literal backslash-n:\n{}",
        s.output()
    );
}

/// The user's typed message echoes into the transcript as a user row before
/// the agent call, proving the input landed as a real user message (not a
/// silent drop) and the transcript order is user → call → fold.
#[test]
#[ignore]
fn test_sync_user_message_echo() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"child result"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("delegatetheauth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    assert!(
        s.output_compact().contains("delegatetheauth"),
        "user message should echo into the transcript:\n{}",
        s.output()
    );
    assert!(
        s.output_compact().contains("Agent(→explore)"),
        "agent call row should follow the user message:\n{}",
        s.output()
    );
}

/// A child that returns unicode text renders it in the fold summary without
/// mangling. Proves the summary path handles non-ascii content.
#[test]
#[ignore]
fn test_sync_child_unicode_summary() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"héllo wörld café"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    assert!(
        s.output_compact().contains("héllo"),
        "summary should show unicode child text:\n{}",
        s.output()
    );
}

/// A very long task prompt does not crash the spawn or the fold render.
#[test]
#[ignore]
fn test_sync_long_prompt_safe() {
    let prompt = "x".repeat(200);
    let script = format!(
        r#"[[{{"type":"ToolCall","id":"toolu_1","name":"agent","input":{{"subagent_type":"explore","prompt":"{prompt}","description":"long"}}}}],[{{"type":"Text","text":"child done"}}],[{{"type":"Text","text":"parent done"}}]]"#
    );
    let mut s = pty_session_scripted(&script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("go");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("parentdone", RENDER_TIMEOUT * 5),
        "run should complete with a long prompt:\n{}",
        s.output()
    );
}

/// A child text containing quotes renders without JSON-escaping (no literal
/// backslash-quote in the summary).
#[test]
#[ignore]
fn test_sync_quotes_unescaped() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"the \"auth\" is here"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    assert!(
        !s.output_compact().contains("\\\""),
        "summary should not show escaped quotes:\n{}",
        s.output()
    );
    assert!(
        s.output_compact().contains("auth"),
        "summary should show the quoted word:\n{}",
        s.output()
    );
}

/// A delegation with no subagent_type defaults to general-purpose, and the
/// banner carries that default label.
#[test]
#[ignore]
fn test_sync_general_purpose_type() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"prompt":"find auth","description":"find auth"}}],
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
        s.output_compact().contains("@general-purpose"),
        "default type should be general-purpose:\n{}",
        s.output()
    );
}
