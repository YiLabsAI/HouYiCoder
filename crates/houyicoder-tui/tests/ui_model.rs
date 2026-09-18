//! /model pane journeys over the real binary: the interactions the unit
//! tests cannot reach, from the Default pick resolving the built-in model and
//! clearing the settings id, through a pick made while a run is streaming,
//! to a pick that reached the session but not disk.

#![allow(clippy::unwrap_in_result)]

mod common;

use common::{Key, PtySession, RENDER_TIMEOUT, fresh_temp_dir, run_slash_command};
use std::path::Path;
use std::time::{Duration, Instant};

/// Poll a predicate at 50ms ticks until it passes or 5s elapse. The pane's
/// catalog reply and the host's settings write are both async, so the test
/// waits on the effect (the file content) rather than a fixed sleep.
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

/// Drop whitespace, so a marker matches the accumulated stream and the
/// reconstructed screen the same way. A redraw only paints the cells that
/// changed, so a blank the pane painted earlier is missing from the stream and
/// a literal space match can miss text that is plainly on screen.
fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// True when the marker shows, whitespace aside, on the reconstructed screen
/// or in the accumulated stream. The screen covers what is visible now; the
/// stream covers lines a later render scrolled out of the viewport.
fn has_text(s: &PtySession, marker: &str) -> bool {
    let want = compact(marker);
    compact(&s.screen().contents()).contains(&want) || compact(&s.output_plain()).contains(&want)
}

/// Poll has_text until it passes or RENDER_TIMEOUT elapses.
fn wait_for_text(s: &mut PtySession, marker: &str) -> bool {
    let deadline = Instant::now() + RENDER_TIMEOUT;
    loop {
        if has_text(s, marker) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Write a settings.json into an isolated home, creating the config dir.
fn seed_settings(home: &Path, json: &str) {
    std::fs::create_dir_all(home.join(".houyicoder")).unwrap();
    std::fs::write(home.join(".houyicoder").join("settings.json"), json).unwrap();
}

/// The settings.json of an isolated home, parsed. Null when unreadable.
fn settings_of(home: &Path) -> serde_json::Value {
    std::fs::read_to_string(home.join(".houyicoder").join("settings.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// The settings.json of an isolated home as text, for a failure message.
fn settings_text(home: &Path) -> String {
    std::fs::read_to_string(home.join(".houyicoder").join("settings.json")).unwrap_or_default()
}

/// Launch into the working screen with an isolated home.
fn launch(home: &Path) -> PtySession {
    let mut s = PtySession::launch_with_home(home.to_path_buf());
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT), "login");
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen: {}",
        s.output_plain()
    );
    s
}

/// Open /model and wait for the pane plus its catalog reply. The footer flips
/// from the empty-state guide to the key hints exactly when the reply lands,
/// so it is the latch for every content assertion below.
fn open_model(s: &mut PtySession) {
    run_slash_command(s, "model");
    assert!(
        wait_for_text(s, "Select a model"),
        "/model pane: {}",
        s.output_plain()
    );
    assert!(
        wait_for_text(s, "Enter to save"),
        "the catalog reply lands: {}",
        s.output_plain()
    );
}

/// The Default row explains itself: it names the model the sentinel resolves
/// to right now, not the settings id the pick is about to clear.
#[test]
#[ignore]
fn test_pane_default_row_target() {
    let home = fresh_temp_dir("model-default-row-home");
    seed_settings(
        &home,
        r#"{"model":{"id":"qwen3-coder","catalog":[{"id":"qwen3-coder"}]}}"#,
    );
    let mut s = launch(&home);
    open_model(&mut s);
    assert!(
        wait_for_text(&mut s, "qwen3.7-max"),
        "the Default row names the model it resolves to: {}",
        s.output_plain()
    );
    assert!(
        has_text(&s, "Default (recommended)"),
        "the Default row keeps its label: {}",
        s.output_plain()
    );
}

/// M-28: picking Default applies the built-in model at once and clears the
/// settings id, so a restart no longer runs the model the pick replaced.
#[test]
#[ignore]
fn test_model_default_applies_default() {
    let home = fresh_temp_dir("model-default-journey-home");
    seed_settings(
        &home,
        r#"{"model":{"id":"qwen3-coder","catalog":[{"id":"qwen3-coder"}]}}"#,
    );
    let mut s = launch(&home);
    open_model(&mut s);
    assert!(
        wait_for_text(&mut s, "qwen3-coder"),
        "the catalog row renders once the reply lands: {}",
        s.output_plain()
    );
    // The draft re-seeds onto the session's model, so Up moves it to Default.
    s.send_key(&Key::Up);
    s.send_key(&Key::Enter);
    assert!(
        wait_for_text(&mut s, "Model set to Default (qwen3.7-max) · medium effort"),
        "the receipt names the resolved default: {}",
        s.output_plain()
    );
    let cleared = wait_for(|| {
        let v = settings_of(&home);
        v["model"].is_object() && v["model"]["id"].as_str().is_none()
    });
    assert!(
        cleared,
        "the Default pick clears model.id from settings.json: {}",
        settings_text(&home)
    );
}

/// M-29: a session launched with --model keeps that model in the pane. The
/// check and the cursor point at the live model, not at the settings id the
/// async catalog reply would otherwise drag them to.
#[test]
#[ignore]
fn test_model_info_respects_session() {
    let home = fresh_temp_dir("model-session-model-home");
    seed_settings(
        &home,
        r#"{"model":{"id":"glm-5.2","catalog":[{"id":"glm-5.2"}]}}"#,
    );
    let mut s = PtySession::launch_with_args(
        None,
        None,
        Some(home),
        None,
        &["--model".to_string(), "qwen3-coder".to_string()],
    );
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT), "login");
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    open_model(&mut s);
    assert!(
        wait_for_text(&mut s, "qwen3-coder"),
        "the session model needs a row of its own: {}",
        s.output_plain()
    );
    let screen = s.screen().contents();
    let focused = screen
        .lines()
        .find(|line| line.contains("qwen3-coder") && line.contains('\u{276f}'))
        .unwrap_or_else(|| panic!("the cursor sits on the session model: {screen}"));
    assert!(
        focused.contains('\u{2714}'),
        "the check sits on the session model, not the settings id: {focused}"
    );
    assert!(
        !screen
            .lines()
            .any(|line| line.contains("glm-5.2") && line.contains('\u{2714}')),
        "the settings id does not carry the check: {screen}"
    );
}

/// M-32: Esc discards the whole draft. Nothing the user adjusted reaches the
/// settings file, and reopening shows the session's real state again.
#[test]
#[ignore]
fn test_picker_esc_discards_draft() {
    let home = fresh_temp_dir("model-esc-home");
    seed_settings(
        &home,
        r#"{"model":{"id":"qwen3-coder","effort_level":"medium","catalog":[{"id":"qwen3-coder"}]}}"#,
    );
    let mut s = launch(&home);
    open_model(&mut s);
    assert!(
        wait_for_text(&mut s, "medium (default)"),
        "the chain's level renders as the default: {}",
        s.output_plain()
    );
    let before = settings_of(&home);
    s.send_key(&Key::Right);
    assert!(
        wait_for_text(&mut s, "Reasoning Effort: high"),
        "Right adjusts the level: {}",
        s.output_plain()
    );
    s.send_key(&Key::Esc);
    assert!(
        wait_for_text(&mut s, "let's build, or / for commands"),
        "Esc closes the pane: {}",
        s.output_plain()
    );
    assert_eq!(
        settings_of(&home),
        before,
        "Esc writes nothing to settings.json"
    );
    open_model(&mut s);
    assert!(
        wait_for_text(&mut s, "medium (default)"),
        "reopening reseeds the level from the chain: {}",
        s.output_plain()
    );
}

/// M-35: a pick made while a request is streaming does not disturb that
/// request, and the receipt names the boundary it lands on instead of
/// implying the running request changed.
#[test]
#[ignore]
fn test_switch_during_run() {
    let home = fresh_temp_dir("model-active-run-home");
    seed_settings(
        &home,
        r#"{"model":{"catalog":[{"id":"glm-5.2","display_name":"Fable"}]}}"#,
    );
    // A long streamed reply holds the request in flight across the whole pick
    // sequence; the delay stretches every 4-char delta of it.
    let plan = format!(r#"[[{{"type":"Text","text":"{}"}}]]"#, "x".repeat(400));
    let mut s = PtySession::launch_with_args(Some(plan), Some(50), Some(home), None, &Vec::new());
    assert!(s.wait_for("sign in to houyicoder", RENDER_TIMEOUT), "login");
    s.send_key(&Key::Char('3'));
    assert!(
        s.wait_for("let's build, or / for commands", RENDER_TIMEOUT),
        "working screen"
    );
    // Start the request, then pick a model while it streams.
    s.send_str("hi");
    s.send_key(&Key::Enter);
    open_model(&mut s);
    assert!(
        wait_for_text(&mut s, "Fable"),
        "the catalog row renders mid-request: {}",
        s.output_plain()
    );
    s.send_key(&Key::Down);
    s.send_key(&Key::Enter);
    assert!(
        wait_for_text(
            &mut s,
            "Model set to Fable (glm-5.2) · medium effort · applies to the next model request"
        ),
        "the receipt names the boundary: {}",
        s.output_plain()
    );
}

/// M-36: selecting Default renders the resolved id in the transcript, so the
/// receipt names the model the session will send instead of the bare choice.
#[test]
#[ignore]
fn test_receipt_names_default() {
    let home = fresh_temp_dir("model-receipt-home");
    seed_settings(
        &home,
        r#"{"model":{"id":"qwen3-coder","catalog":[{"id":"qwen3-coder"}]}}"#,
    );
    let mut s = launch(&home);
    open_model(&mut s);
    assert!(
        wait_for_text(&mut s, "qwen3-coder"),
        "the catalog arrives: {}",
        s.output_plain()
    );
    s.send_key(&Key::Up);
    s.send_key(&Key::Enter);
    assert!(
        wait_for_text(&mut s, "Model set to Default (qwen3.7-max) · medium effort"),
        "the Default receipt names the model: {}",
        s.output_plain()
    );
    assert!(
        !has_text(&s, "model: Default"),
        "no receipt without the actual id: {}",
        s.output_plain()
    );
}

/// M-37: an explicit pick renders the display name, the real id, the effort
/// the next request carries and the Fast tier, all from the host's reply.
#[test]
#[ignore]
fn test_receipt_names_picked_model() {
    let home = fresh_temp_dir("model-receipt-id-home");
    seed_settings(
        &home,
        r#"{"model":{"effort_level":"high","catalog":[{"id":"qwen3.8-max","display_name":"Max","fast":true}]}}"#,
    );
    let mut s = launch(&home);
    open_model(&mut s);
    assert!(
        wait_for_text(&mut s, "Max"),
        "the catalog arrives: {}",
        s.output_plain()
    );
    // Focus the catalog row, move to the Fast setting, turn it on, commit once.
    s.send_key(&Key::Down);
    assert!(
        wait_for_text(&mut s, "Reasoning Effort: high (default)"),
        "the chain's level follows the focused row: {}",
        s.output_plain()
    );
    s.send_key(&Key::Tab);
    s.send_key(&Key::Right);
    assert!(
        wait_for_text(&mut s, "Fast Mode: on"),
        "Fast is adjustable on a model that serves it: {}",
        s.output_plain()
    );
    s.send_key(&Key::Enter);
    assert!(
        wait_for_text(
            &mut s,
            "Model set to Max (qwen3.8-max) · high effort · Fast mode on"
        ),
        "the receipt names the pick and the settings that apply: {}",
        s.output_plain()
    );
}

/// M-38: a pick that reached the session but not disk says so. The receipt
/// spells out the partial outcome instead of claiming a clean save.
#[cfg(unix)]
#[test]
#[ignore]
fn test_failure_reports_not_saved() {
    use std::os::unix::fs::PermissionsExt;
    let home = fresh_temp_dir("model-partial-home");
    seed_settings(
        &home,
        r#"{"model":{"catalog":[{"id":"glm-5.2","display_name":"Fable"}]}}"#,
    );
    let mut s = launch(&home);
    // Make the config dir unwritable so the settings write cannot land: the
    // session still switches, so the receipt must report the partial result.
    let cfg = home.join(".houyicoder");
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o555)).unwrap();
    open_model(&mut s);
    assert!(
        wait_for_text(&mut s, "Fable"),
        "the catalog arrives: {}",
        s.output_plain()
    );
    s.send_key(&Key::Down);
    s.send_key(&Key::Enter);
    assert!(
        wait_for_text(&mut s, "settings not saved"),
        "the receipt names the destination that failed: {}",
        s.output_plain()
    );
    // Restore so the harness can clean the temp dir up.
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A catalog entry the provider does not serve surfaces as a startup system
/// line, so the user sees the stale id or typo. Seeds a served-models cache
/// (as a prior fetch would leave) plus a settings catalog carrying the stale
/// id, then asserts the warning reaches the transcript on launch.
#[test]
#[ignore]
fn test_stale_catalog_warns_startup() {
    let home = fresh_temp_dir("stale-model-warn-home");
    let cfg = home.join(".houyicoder");
    std::fs::create_dir_all(cfg.join("cache")).unwrap();
    std::fs::write(
        cfg.join("cache").join("served-models.json"),
        r#"{"ids":["qwen3-coder","glm-5.2"],"timestamp":1700000000}"#,
    )
    .unwrap();
    std::fs::write(
        cfg.join("settings.json"),
        r#"{"model":{"catalog":[{"id":"qwen3.8-max"}]}}"#,
    )
    .unwrap();
    let mut s = launch(&home);
    assert!(
        wait_for_text(
            &mut s,
            "qwen3.8-max is not in the provider's served-model list"
        ),
        "stale catalog id warns on startup: {}",
        s.output_plain()
    );
}

/// When settings.json has no catalog (the out-of-box state), the pane falls
/// back to the shipped catalog so the user sees the tiers without configuring
/// anything.
#[test]
#[ignore]
fn test_empty_settings_shows_catalog() {
    let home = fresh_temp_dir("default-catalog-home");
    let mut s = launch(&home);
    open_model(&mut s);
    assert!(
        wait_for_text(&mut s, "Max"),
        "the default catalog row Max renders: {}",
        s.output_plain()
    );
    let out = s.output_plain();
    for name in ["Max", "Fable", "Pro", "Flash"] {
        assert!(out.contains(name), "{name} row visible: {out}");
    }
}

/// --model <id> overrides the settings model for a fresh session: the status
/// bar carries the flag's id, not the settings id.
#[test]
#[ignore]
fn test_model_flag_overrides_settings() {
    let home = fresh_temp_dir("model-flag-home");
    seed_settings(
        &home,
        r#"{"model":{"id":"qwen3-coder","catalog":[{"id":"qwen3-coder"}]}}"#,
    );
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
    assert!(
        wait_for_text(&mut s, "glm-5.2"),
        "--model overrides the settings model in the status bar: {}",
        s.output_plain()
    );
    assert!(
        !has_text(&s, "qwen3-coder"),
        "the settings id does not appear (the flag won): {}",
        s.output_plain()
    );
}

/// Enter on a catalog model writes model.id and the adjusted effort, so the
/// pick survives a restart. Drives the real pane keys: Down to move, Left to
/// adjust the level, Enter to commit.
#[test]
#[ignore]
fn test_model_enter_persists_pick() {
    let home = fresh_temp_dir("model-enter-persist-home");
    seed_settings(&home, r#"{"model":{"catalog":[{"id":"qwen3-coder"}]}}"#);
    let mut s = launch(&home);
    open_model(&mut s);
    assert!(
        wait_for_text(&mut s, "qwen3-coder"),
        "the catalog row renders once the reply lands: {}",
        s.output_plain()
    );
    // The draft re-seeds onto the session's model, which here is the built-in
    // default: Down moves onto the catalog row.
    s.send_key(&Key::Down);
    // Right steps the effort off the auto base up to low; the default marker
    // drops because the draft no longer follows the chain.
    s.send_key(&Key::Right);
    assert!(
        wait_for_text(&mut s, "Reasoning Effort: low"),
        "Left adjusts the level on a qwen3 row: {}",
        s.output_plain()
    );
    s.send_key(&Key::Enter);
    assert!(
        wait_for_text(
            &mut s,
            "Model set to qwen3-coder (qwen3-coder) · low effort"
        ),
        "the receipt names the picked model and its id: {}",
        s.output_plain()
    );
    let persisted = wait_for(|| {
        let v = settings_of(&home);
        v["model"]["id"].as_str() == Some("qwen3-coder")
            && v["model"]["catalog"][0]["effort"].as_str() == Some("low")
    });
    assert!(
        persisted,
        "Enter persisted model.id + effort to settings.json: {}",
        settings_text(&home)
    );
}
