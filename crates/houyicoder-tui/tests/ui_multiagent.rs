//! Real-binary PTY tests for the multi-agent interaction UX. Each test
//! spawns the binary under a PTY with a scripted stub provider so the full
//! chain runs end-to-end. The matrix covers each interaction path a user
//! can take. Slow, so ignored by default.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, pty_session_scripted, pty_session_slow_scripted};
use std::time::Duration;

/// A child text long enough that the collapsed fold summary truncates it.
/// Head and tail are distinct so a collapsed/expanded assertion can tell
/// summary from full content.
const LONG_CHILD: &str = "This is a long child analysis that exceeds the one-line fold summary limit so the collapsed head truncates it with an ellipsis while the expanded view shows the full text including this trailing sentinel.";

/// The grace window after which a completed pill row retires when the user
/// is not viewing it. Tests wait past this to prove the pin holds + the
/// post-exit retire fires. Mirrors the FLEET_GRACE constant in agent_message.
const FLEET_GRACE: Duration = Duration::from_secs(5);

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
    // the viewed child's turn). The working-screen placeholder returning
    // confirms the banner is gone.
    s.send_key(&Key::ShiftDown);
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "after Shift+Down, should return to parent transcript:\n{}",
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

/// A delegation in the transcript must not swallow Ctrl+O for the blocks
/// that follow it. The delegation runs first, then the parent answers with
/// reasoning, so the reasoning summary is the latest expandable block and the
/// key belongs to it. Delegation expand used to answer for every Ctrl+O
/// whatever the cursor pointed at, which left reasoning expandable by mouse
/// and impossible to collapse by keyboard. Only the real terminal proves the
/// key reaches the routing at all — a unit test calling the handler directly
/// skips the dispatch this pins.
#[test]
#[ignore]
fn test_multi_ctrl_o_thought() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"auth is in src/auth"}],
        [{"type":"Reasoning","text":"weighing the auth options"},{"type":"Text","text":"all done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("Thoughtfor", RENDER_TIMEOUT * 3),
        "the reasoning summary should render after the delegation:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for_compact("weighingtheauthoptions", RENDER_TIMEOUT),
        "Ctrl+O should expand the reasoning, not the earlier delegation:\n{}",
        s.output()
    );
    assert!(
        !s.output_compact().contains("childtranscript"),
        "the delegation should stay collapsed:\n{}",
        s.output()
    );
    s.clear_output();
    s.send_key(&Key::Ctrl('o'));
    assert!(
        s.wait_for_compact("expand)", RENDER_TIMEOUT),
        "a second Ctrl+O should offer expand again:\n{}",
        s.output()
    );
    assert!(
        !s.output_compact().contains("weighingtheauthoptions"),
        "collapsing should drop the reasoning body:\n{}",
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
    s.send_key(&Key::ShiftDown);
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "Shift+Down should exit the teammate view (the view stayed until \
         Shift+Down, not auto-dismissed on completion):\n{}",
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

/// Two consecutive sync delegations produce two independent fold-groups,
/// each carrying its own child summary. Proves per-child rendering (not a
/// merged or last-writer-wins fold).
#[test]
#[ignore]
fn test_sync_two_folds_render() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"first","description":"first"}},
         {"type":"ToolCall","id":"toolu_2","name":"agent","input":{"subagent_type":"explore","prompt":"second","description":"second"}}],
        [{"type":"Text","text":"first-child-result"}],
        [{"type":"Text","text":"second-child-result"}],
        [{"type":"Text","text":"parent done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("delegate two");
    s.send_str("\r");
    assert!(
        s.wait_for_compact("parentdone", RENDER_TIMEOUT * 3),
        "both children should complete + parent resume:\n{}",
        s.output()
    );
    let pc = s.output_compact();
    assert!(
        pc.contains("first-child-result") && pc.contains("second-child-result"),
        "both child summaries should render:\n{}",
        s.output()
    );
}

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

/// A long child text is truncated in the collapsed summary: the head shows
/// but the tail does not. Proves the summary caps at one row.
#[test]
#[ignore]
fn test_fold_summary_truncates() {
    let script = format!(
        r#"[
        [{{"type":"ToolCall","id":"toolu_1","name":"agent","input":{{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}}}],
        [{{"type":"Text","text":"{LONG_CHILD}"}}],
        [{{"type":"Text","text":"done"}}]
    ]"#
    );
    let mut s = pty_session_scripted(&script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    let pc = s.output_compact();
    assert!(
        pc.contains("longchildanalysis"),
        "summary head should show the start:\n{}",
        s.output()
    );
    assert!(
        !pc.contains("trailingsentinel"),
        "collapsed summary should not show the tail:\n{}",
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

/// A single Esc exits the teammate view back to the parent transcript.
/// Proves the exit is one press (not two), distinct from the slash-palette
/// path which needs a second Esc.
#[test]
#[ignore]
fn test_teammate_esc_single() {
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
    assert!(
        s.wait_for("let's build", RENDER_TIMEOUT),
        "single Esc should exit the teammate view:\n{}",
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

/// The fold-group head shows the subagent_type label, not a generic
/// placeholder. Uses the registered "plan" type to prove the label is not
/// hard-coded to one value.
#[test]
#[ignore]
fn test_sync_fold_shows_type() {
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
    assert!(
        s.output_compact().contains("plan:"),
        "fold head should show the subagent type:\n{}",
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

/// Two delegations in one run with distinct subagent types each render
/// their own type label in the fold head (explore + plan), proving the
/// per-child type is not lost when multiple delegations land together.
#[test]
#[ignore]
fn test_two_folds_distinct_types() {
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
    assert!(s.wait_for_compact("parentdone", RENDER_TIMEOUT * 3));
    let pc = s.output_compact();
    assert!(
        pc.contains("explore:") && pc.contains("plan:"),
        "both fold heads should show their own type:\n{}",
        s.output()
    );
}

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

/// A short child text appears fully in the collapsed summary (no truncation
/// ellipsis for content under the one-line cap).
#[test]
#[ignore]
fn test_fold_short_summary_full() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"found it"}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    assert!(
        s.output_compact().contains("foundit"),
        "short child text should appear fully in the summary:\n{}",
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

/// The parent's final answer renders after the fold-group (the delegation
/// result precedes the parent's resume text in transcript order).
#[test]
#[ignore]
fn test_sync_parent_after_fold() {
    let script = r#"[
        [{"type":"ToolCall","id":"toolu_1","name":"agent","input":{"subagent_type":"explore","prompt":"find auth","description":"find auth"}}],
        [{"type":"Text","text":"child found auth"}],
        [{"type":"Text","text":"parent final answer"}]
    ]"#;
    let mut s = pty_session_scripted(script);
    assert!(s.wait_for("let's build", RENDER_TIMEOUT));
    s.send_str("find auth");
    s.send_str("\r");
    assert!(s.wait_for_compact("ctrl+otoexpand", RENDER_TIMEOUT * 2));
    assert!(
        s.output_compact().contains("parentfinalanswer"),
        "parent final answer should render after the fold:\n{}",
        s.output()
    );
    assert!(
        s.output_compact().contains("childfoundauth"),
        "child result should render in the fold:\n{}",
        s.output()
    );
}
/// The /agents pane lists this session's returned delegations once the
/// footer strip has retired them: a returned delegation is durable history,
/// so the pane (the record surface) keeps it after the strip (the present
/// tense) drops it. Selectable, Enter opens the delegation's view.
#[test]
fn test_agents_pane_lists_returned() {
    use houyicoder_tui::records::TranscriptLine;
    let mut app = houyicoder_tui::composition::app();
    app.screen = houyicoder_tui::state::Screen::Working;
    app.pane = houyicoder_tui::state::Pane::Agents;
    app.agent_directory = None;
    app.push_transcript_line(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
        color: None,
    });
    let v = app.transcript_version.get();
    app.agents.refresh(&app.transcript, v);
    assert_eq!(
        app.agents.rows.len(),
        1,
        "the returned delegation is listed"
    );
    assert!(app.agents.rows[0].loaded, "loaded fold detected");
    // Enter on the selected row opens that delegation's view.
    houyicoder_tui::keys::handle_working(
        &mut app,
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ),
    );
    let view = app.teammate_view.as_ref().expect("view opened");
    assert_eq!(view.child_sid, "c1");
    assert!(
        !view.transcript.is_empty(),
        "the loaded fold is copied into the view"
    );
}
/// Arrows walk the returned-delegation list when the fleet is retired, and
/// the cursor clamps at the bounds.
#[test]
fn test_agents_pane_cursor_walks() {
    use houyicoder_tui::records::TranscriptLine;
    let mut app = houyicoder_tui::composition::app();
    app.screen = houyicoder_tui::state::Screen::Working;
    app.pane = houyicoder_tui::state::Pane::Agents;
    for (sid, summary) in [("c1", "first"), ("c2", "second")] {
        app.push_transcript_line(TranscriptLine::Subagent {
            child_sid: sid.into(),
            subagent_type: "explore".into(),
            summary: summary.into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    }
    let v = app.transcript_version.get();
    app.agents.refresh(&app.transcript, v);
    app.agents.move_selection(1);
    assert_eq!(app.agents.sel, 1);
    app.agents.move_selection(1);
    assert_eq!(app.agents.sel, 1, "clamped at the last row");
    app.agents.move_selection(-1);
    assert_eq!(app.agents.sel, 0, "clamped at the first row");
}
