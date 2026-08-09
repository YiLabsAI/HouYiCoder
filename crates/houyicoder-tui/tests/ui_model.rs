//! /model Enter persistence PTY journey test: selecting a catalog model +
//! cycling effort to low, then pressing Enter, writes model.id + the
//! per-model effort to settings.json so the pick survives a restart. Drives
//! the real binary with an isolated HOME so the settings file the test owns
//! is the one the binary reads + writes.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, run_slash_command};
use std::time::{Duration, Instant};

/// Poll a predicate at 50ms ticks until it passes or 5s elapse. The model
/// pane's catalog + the server's persist are both async on the shared
/// runtime; the test drives them through the real binary, so it waits on
/// observable output rather than a fixed sleep.
fn wait_for<F: FnMut() -> bool>(mut pred: F) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// /model Enter on a catalog model writes model.id; cycling effort to low
/// writes the per-model effort. The settings file (under the isolated HOME)
/// carries both after Enter, so a restart restores the pick. Exercises the
/// persist_model_pick path end-to-end through the real pane keys (Down to
/// move, Left to cycle effort backward, Enter to select).
#[test]
#[ignore]
fn test_model_enter_persists_pick() {
    let home = fresh_temp_dir("model-enter-persist-home");
    // Seed a catalog with a qwen3-family model so the effort selector is
    // active (supports_effort matches qwen3). No effort field yet — Enter
    // with effort=low writes it.
    std::fs::create_dir_all(home.join(".houyicoder")).unwrap();
    let settings = home.join(".houyicoder").join("settings.json");
    std::fs::write(&settings, r#"{"model":{"catalog":[{"id":"qwen3-coder"}]}}"#).unwrap();

    let mut s = PtySession::launch_with_home(home.clone());
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT), "login");
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    run_slash_command(&mut s, "model");
    assert!(
        s.wait_for_plain("Select a model", RENDER_TIMEOUT),
        "/model pane: {}",
        s.output_plain()
    );
    // The catalog rows arrive async (the ModelInfo reply lands after the pane
    // opens; until then the empty-state guide shows). Wait for the seeded row.
    assert!(
        s.wait_for_plain("qwen3-coder", RENDER_TIMEOUT),
        "seeded catalog row renders once ModelInfo lands: {}",
        s.output_plain()
    );
    // Down: cursor off Default onto the qwen3-coder row. The effort selector
    // recomputes to Medium (the supported-model default) — [medium] bracketed
    // confirms the cursor is on a model that speaks an effort dialect.
    s.send_key(&Key::Down);
    assert!(
        s.wait_for_plain("[medium]", RENDER_TIMEOUT),
        "effort selector shows medium active on the qwen3 row: {}",
        s.output_plain()
    );
    // Left: cycle effort backward Medium -> Low (sets toggled=true).
    s.send_key(&Key::Left);
    assert!(
        s.wait_for_plain("[low]", RENDER_TIMEOUT),
        "effort cycled to low: {}",
        s.output_plain()
    );
    // Enter: set_model_at_cursor ships ModelSwitch + closes the pane + emits
    // a "model: <id>" system line. The system line confirms the client sent
    // the switch; the file write is the server's async persist.
    s.send_key(&Key::Enter);
    assert!(
        s.wait_for_plain("model: qwen3-coder", RENDER_TIMEOUT),
        "set_model_at_cursor fired the model: system line: {}",
        s.output_plain()
    );
    // The server's persist_model_pick writes settings.json async; poll the
    // file for the concrete id + the cycled effort.
    let persisted = wait_for(|| {
        std::fs::read_to_string(&settings)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .is_some_and(|v| {
                v["model"]["id"].as_str() == Some("qwen3-coder")
                    && v["model"]["catalog"][0]["effort"].as_str() == Some("low")
            })
    });
    assert!(
        persisted,
        "Enter persisted model.id + effort to settings.json: {}",
        std::fs::read_to_string(&settings).unwrap_or_default()
    );

    // Restart: relaunch the binary with the same isolated HOME. resolve_model
    // reads model.id from the persisted settings, so the status bar carries
    // the picked model — the cross-session survival the write enables.
    let mut s2 = PtySession::launch_with_home(home);
    assert!(
        s2.wait_for("sign in to houyicoder", RENDER_TIMEOUT),
        "login on restart"
    );
    s2.send_key(&Key::Char('3'));
    assert!(
        s2.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen on restart"
    );
    assert!(
        s2.wait_for_plain("qwen3-coder", RENDER_TIMEOUT),
        "the persisted model surfaces in the status bar after restart: {}",
        s2.output_plain()
    );
}

/// A catalog entry the provider does not serve surfaces as a startup system
/// line so the user sees the stale id or typo (the substring name-match in
/// supports_effort cannot catch a non-existent version like qwen3.8-max).
/// Seeds a served-models cache (as if a prior /v1/models fetch landed) +
/// a settings catalog with the stale id, then asserts the warning reaches
/// the transcript on launch. Stub mode (empty keys) keeps the FakeProvider
/// refresh a no-op so the seeded cache survives + the warning fires from
/// validate_catalog at build time. Covers the read-then-warn path the unit
/// tests cannot (cached_ids is empty under cfg(test)).
#[test]
#[ignore]
fn test_stale_catalog_warns_startup() {
    let home = fresh_temp_dir("stale-model-warn-home");
    let cfg = home.join(".houyicoder");
    std::fs::create_dir_all(cfg.join("cache")).unwrap();
    // Seed the served-models cache as a prior /v1/models fetch would have.
    std::fs::write(
        cfg.join("cache").join("served-models.json"),
        r#"{"ids":["qwen3-coder","glm-5.2"],"timestamp":1700000000}"#,
    )
    .unwrap();
    // Catalog with a stale id the provider does not serve (and the substring
    // name-match cannot flag — "qwen3.8-max" does not contain "qwen3-coder").
    std::fs::write(
        cfg.join("settings.json"),
        r#"{"model":{"catalog":[{"id":"qwen3.8-max"}]}}"#,
    )
    .unwrap();

    let mut s = PtySession::launch_with_home(home);
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT), "login");
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    assert!(
        s.wait_for_plain(
            "qwen3.8-max is not in the provider's served-model list",
            RENDER_TIMEOUT,
        ),
        "stale catalog id warns on startup: {}",
        s.output_plain()
    );
}

/// When settings.json has no catalog (the out-of-box state), the /model pane
/// falls back to the shipped DEFAULT_CATALOG so the user sees Max/Fable/Pro/
/// Flash without configuring anything. Pins the fallback end-to-end through
/// the real binary (the ModelInfo wire reply carries the default entries).
#[test]
#[ignore]
fn test_empty_settings_shows_catalog() {
    let home = fresh_temp_dir("default-catalog-home");
    // No settings.json at all — the pane should show the shipped defaults.
    let mut s = PtySession::launch_with_home(home);
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT), "login");
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    run_slash_command(&mut s, "model");
    assert!(
        s.wait_for_plain("Select a model", RENDER_TIMEOUT),
        "/model pane: {}",
        s.output_plain()
    );
    // The catalog rows arrive async (the ModelInfo reply lands after the
    // pane opens). Wait for the first default entry before asserting.
    assert!(
        s.wait_for_plain("Max", RENDER_TIMEOUT),
        "default catalog row Max renders: {}",
        s.output_plain()
    );
    let out = s.output_plain();
    assert!(out.contains("Max"), "Max row visible: {out}");
    assert!(out.contains("Fable"), "Fable row visible: {out}");
    assert!(out.contains("Pro"), "Pro row visible: {out}");
    // Flash is a designed tier but not yet in the shipped default catalog —
    // add it to DEFAULT_CATALOG (model_section.rs) when the Flash model id
    // is known, then re-add the assertion here.
}

/// --model <id> overrides the settings.json model for a fresh session: the
/// status bar carries the flag's id, not the settings id. Slots above
/// settings in the resolution chain (below a resumed session's sidecar).
#[test]
#[ignore]
fn test_model_flag_overrides_settings() {
    let home = fresh_temp_dir("model-flag-home");
    std::fs::create_dir_all(home.join(".houyicoder")).unwrap();
    // Settings has a catalog with qwen3-coder as the active id; --model glm-5.2
    // should win, so the status bar carries glm-5.2, not qwen3-coder.
    std::fs::write(
        home.join(".houyicoder").join("settings.json"),
        r#"{"model":{"id":"qwen3-coder","catalog":[{"id":"qwen3-coder"}]}}"#,
    )
    .unwrap();
    let mut s = PtySession::launch_with_args(
        None,
        None,
        Some(home),
        None,
        &["--model".to_string(), "glm-5.2".to_string()],
    );
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT), "login");
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    // The flag's id leads the status bar (the runner + the display both
    // carry the override, not the settings id).
    assert!(
        s.wait_for_plain("glm-5.2", RENDER_TIMEOUT),
        "--model overrides settings model in the status bar: {}",
        s.output_plain()
    );
    assert!(
        !s.output_plain().contains("qwen3-coder"),
        "the settings id qwen3-coder should not appear (the flag won): {}",
        s.output_plain()
    );
}
