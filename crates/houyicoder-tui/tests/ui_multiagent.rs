//! Real-binary PTY tests for the multi-agent sync delegation UX.
//! Each test spawns the houyi binary under a PTY with a scripted stub
//! provider, so the full chain runs end-to-end: the parent calls the
//! agent tool, the sync spawn drives the child, the child completes, the
//! parent resumes, and the Subagent fold-group + teammate view render
//! through the real interaction layer. Slow, so ignored by default.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, session_on_working_with_script};
use std::time::Duration;

/// The grace window after which a completed pill row retires when the user
/// is not viewing it. Tests wait past this to prove the pin holds + the
/// post-exit retire fires. Mirrors the FLEET_GRACE constant in agent_message.
const FLEET_GRACE: Duration = Duration::from_secs(5);

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
    // Wait for the fold-group hint, not "explore" (the footer pill renders
    // "explore: ..." at spawn + would match before the child completes).
    assert!(
        s.wait_for_plain("ctrl+o", RENDER_TIMEOUT * 2),
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
    let mut s = session_on_working_with_script(script);
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
    // active state (Esc exits, which only happens from inside the view).
    std::thread::sleep(FLEET_GRACE + Duration::from_secs(2));
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "Esc should exit the teammate view (the view stayed until Esc, not \
         auto-dismissed on completion):\n{}",
        s.output()
    );
}

/// A slash typed inside the teammate view routes to the parent command
/// palette, not the child's inbox: typing "/" while viewing a child opens
/// the parent slash palette (the command list), proving the slash did not
/// route as a steering message to the child. Esc closes the palette, then
/// Esc exits the teammate view. Slow, ignored by default.
#[test]
#[ignore]
fn test_teammate_slash_routes_parent() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth is in src/auth"}],
        [{"type":"Text","text":"the auth module is in src/auth"}]
    ]"#;
    let mut s = session_on_working_with_script(script);
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
    // Esc closes the palette; a second Esc exits the teammate view.
    s.send_key(&Key::Esc);
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "Esc should close the palette + exit the teammate view:\n{}",
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
    let mut s = session_on_working_with_script(script);
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
