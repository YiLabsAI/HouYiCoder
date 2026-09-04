//! Real-binary PTY smoke test for the /hooks pane. #[ignore] (each spawns
//! the houyi binary + a PTY -- too slow for the commit gate). Run via
//! make test ui (builds the bin first) or
//! cargo test --test ui_hooks -- --ignored after cargo build --bin houyi.
//!
//! Industrial-usability proof for the hooks surface: open the pane and
//! assert the read-only subtitle ("N hooks configured") + the settings
//! hint render. The pane is read-only inspection of the framework +
//! configured hook events; configuration lives in settings.json, so the
//! hint points there. The inline unit layer (configured_count, sort +
//! detail index, event description) proves the logic; this layer proves a
//! user can open the pane and see the subtitle + hint, and that Esc
//! dismisses it.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, RENDER_TIMEOUT, pty_session, run_slash_command};

/// /hooks opens the pane and renders the "N hooks configured" subtitle +
/// the "edit settings.json to configure" hint. Esc at the event-list level
/// dismisses the pane back to the working screen (Esc at the detail level
/// steps back to the list first).
#[test]
#[ignore]
fn test_hooks_pane_subtitle_hint() {
    let mut s = pty_session();
    run_slash_command(&mut s, "hooks");
    assert!(
        s.wait_for("hooks configured", RENDER_TIMEOUT),
        "hooks pane subtitle should render:\n{}",
        s.output()
    );
    assert!(
        s.output().contains("edit settings.json"),
        "the read-only hint should point at settings.json:\n{}",
        s.output()
    );
    // Esc dismisses the pane back to the working screen.
    s.send_key(&Key::Esc);
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "Esc should close the hooks pane back to working:\n{}",
        s.output()
    );
    drop(s);
}

// ---- hook fire-point PTY tests ----

/// Launch the binary with an isolated HOME whose settings.json carries
/// the given hooks config, plus a stub provider script. Pre-trusts the
/// workspace cwd so User-source hooks are not skipped by the trust gate.
/// Returns a logged-in session at the working screen.
fn launch_with_hooks(hooks_json: serde_json::Value, stub_script: &str) -> common::PtySession {
    let home = common::fresh_temp_dir("hook-home");
    let config_dir = home.join(".houyicoder");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    let settings = config_dir.join("settings.json");
    let settings_json = serde_json::json!({ "hooks": hooks_json });
    std::fs::write(
        &settings,
        serde_json::to_string_pretty(&settings_json).unwrap(),
    )
    .expect("write settings");
    let cwd = std::env::current_dir().expect("cwd");
    houyicoder_config::persist_project_trust(&settings, &cwd).expect("pre-trust cwd");
    let mut s = common::PtySession::launch_with_args(
        Some(stub_script.to_string()),
        None,
        Some(home.clone()),
        None,
        &[],
    );
    assert!(
        s.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "login screen should render:\n{}",
        s.output()
    );
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen should render:\n{}",
        s.output()
    );
    s
}

/// A deny verdict from a PreToolUse hook surfaces in the transcript.
/// The hook is a shell script that always denies; the stub provider
/// drives a Bash tool call; the deny reason renders before the tool runs.
#[test]
#[ignore]
fn test_pretooluse_deny_blocks_tool() {
    let deny_script = r#"cat >/dev/null; printf '{"verdict":"deny","reason":"no bash allowed"}'"#;
    let hooks = serde_json::json!({
        "PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type":"command","command": deny_script}]}
        ]
    });
    let script = r#"[
        [{"type":"ToolCall","id":"c1","name":"bash","input":{"command":"echo hi"}}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = launch_with_hooks(hooks, script);
    s.send_str("run echo hi");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("no bash allowed", RENDER_TIMEOUT),
        "deny reason should render:\n{}",
        s.output()
    );
    drop(s);
}

/// An allow verdict from a PreToolUse hook lets the tool run. The hook
/// returns allow; the Bash tool executes and its output renders.
#[test]
#[ignore]
fn test_pretooluse_allow_permits_tool() {
    let allow_script = r#"cat >/dev/null; printf '{"verdict":"allow"}'"#;
    let hooks = serde_json::json!({
        "PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type":"command","command": allow_script}]}
        ]
    });
    let script = r#"[
        [{"type":"ToolCall","id":"c1","name":"bash","input":{"command":"echo hello-from-bash"}}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = launch_with_hooks(hooks, script);
    s.send_str("run echo");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("hello-from-bash", RENDER_TIMEOUT),
        "tool output should render when hook allows:\n{}",
        s.output()
    );
    drop(s);
}

/// A matcher that does not match the tool name skips the hook. The hook
/// script would deny, but the matcher filters it out so the tool runs.
#[test]
#[ignore]
fn test_matcher_mismatch_skips_hook() {
    let deny_script = r#"cat >/dev/null; printf '{"verdict":"deny","reason":"should not fire"}'"#;
    let hooks = serde_json::json!({
        "PreToolUse": [
            {"matcher": "Edit", "hooks": [{"type":"command","command": deny_script}]}
        ]
    });
    let script = r#"[
        [{"type":"ToolCall","id":"c1","name":"bash","input":{"command":"echo ok"}}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = launch_with_hooks(hooks, script);
    s.send_str("run echo");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("ok", RENDER_TIMEOUT),
        "tool should run when matcher does not match:\n{}",
        s.output()
    );
    assert!(
        !s.output().contains("should not fire"),
        "deny hook should not fire on non-matching matcher"
    );
    drop(s);
}

/// An if-condition with a content pattern that matches the tool's
/// primary content field fires the hook. Bash(git *) matches when
/// input.command starts with "git".
#[test]
#[ignore]
fn test_if_condition_matches_content() {
    let deny_script = r#"cat >/dev/null; printf '{"verdict":"deny","reason":"ifcond-matched"}'"#;
    // The if-condition Bash(echo *) matches when input.command starts
    // with "echo". The tool input must pass the permission gate first,
    // so use "echo hi" (a safe command) rather than "git push" (which
    // the default permission rules block before the hook fires).
    let hooks = serde_json::json!({
        "PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type":"command","command": deny_script, "if": "Bash(echo *)"}]}
        ]
    });
    let script = r#"[
        [{"type":"ToolCall","id":"c1","name":"bash","input":{"command":"echo hi"}}],
        [{"type":"Text","text":"done"}]
    ]"#;
    let mut s = launch_with_hooks(hooks, script);
    s.send_str("run echo hi");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("ifcond-matched", RENDER_TIMEOUT),
        "if-condition match should fire hook:\n{}",
        s.output()
    );
    drop(s);
}

/// A PostToolUse hook fires after the tool completes. The hook observes
/// the tool result and its verdict surfaces in the transcript.
#[test]
#[ignore]
fn test_posttooluse_hook_fires() {
    let observe_script =
        r#"cat >/dev/null; printf '{"verdict":"observe","content":"post-tool observed"}'"#;
    let hooks = serde_json::json!({
        "PostToolUse": [
            {"matcher": "Bash", "hooks": [{"type":"command","command": observe_script}]}
        ]
    });
    let script = r#"[
        [{"type":"ToolCall","id":"c1","name":"bash","input":{"command":"echo done"}}],
        [{"type":"Text","text":"finished"}]
    ]"#;
    let mut s = launch_with_hooks(hooks, script);
    s.send_str("run echo");
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for("post-tool observed", RENDER_TIMEOUT),
        "PostToolUse hook observe should render:\n{}",
        s.output()
    );
    drop(s);
}
