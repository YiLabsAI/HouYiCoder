use super::approval::handle_approval;
use super::input::{cycle_pane, handle_input};
use super::palette::handle_palette;
use super::*;
use crate::agent_message::FleetEntry;
use crate::composition;
use crate::composition::app;
use crate::evidence::Verdict;
use crate::input::queue::InputItem;
use crate::pending_prompt::PendingPrompt;
use crate::records::TeammateView;
use crate::records::{Approval, TranscriptLine};
use crate::state::Screen;
use crate::state::enums::StatusTab;
use crate::state::model_draft::ModelSettingFocus;
use crate::test_harness::attach_connection;
use crate::test_harness::render_text;
use crate::test_harness::{
    TransportEvent, connected_app_with_events, model_app, model_caps, model_catalog, model_entry,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::LoginMode;
use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::model::{ModelCatalog, ModelChoice, SpeedMode};
use houyicoder_protocol::frontend::skills::SkillEntry;
use houyicoder_protocol::llm::EffortLevel;
use std::time::{Duration, Instant};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn working_app() -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app
}
fn catalog_app(rows: &[(&str, bool, bool)]) -> App {
    let entries = rows
        .iter()
        .map(|(id, effort, fast)| model_entry(id, id, model_caps(*effort, *fast)))
        .collect();
    let head = rows.first().map(|r| r.0).unwrap_or_default().to_string();
    let mut catalog = ModelCatalog {
        entries,
        ..ModelCatalog::default()
    };
    catalog.applied.id = head.clone();
    catalog.resolved_default.id = head.clone();
    catalog.selected = ModelChoice::Explicit { id: head };
    // A global fallback level, so a row without a per-model pick resolves to
    // the chain rather than to nothing.
    catalog.effort_level = Some(EffortLevel::Medium);
    model_app(catalog)
}

/// The /model pane Up/Down/Enter keys: the draft opens on the row the session
/// runs, Down moves the focus down the list, Up moves back, and a disconnected
/// Enter reports not connected, keeps the pane open, and leaves the applied
/// model untouched.
#[test]
fn test_model_pane_keys() {
    let mut app = catalog_app(&[("a", true, true), ("b", true, true), ("c", true, true)]);
    app.ui.navigation.pane = Pane::Model;
    assert_eq!(
        app.ui.model.row, 1,
        "the draft opens on the session's model"
    );
    handle_input(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.model.row, 2, "Down moves the focus");
    handle_input(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.model.row, 1, "Up moves back");
    let applied = app
        .sessions
        .current
        .capabilities
        .model
        .catalog
        .applied
        .clone();
    handle_input(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.sessions.current.capabilities.model.catalog.applied, applied,
        "disconnected Enter keeps the applied model"
    );
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Model,
        "disconnected Enter keeps the pane open"
    );
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("model: not connected"))),
        "disconnected Enter reports not connected"
    );
}

/// Moving the model focus re-resolves the level from that row's chain. A level
/// the user adjusted on one row is not carried onto another: the chain owns
/// the per-model value, not a draft-wide toggle.
#[test]
fn test_model_pane_reseeds_effort() {
    let mut app = catalog_app(&[("qwen3.7-max", true, true), ("deepseek-chat", false, true)]);
    app.ui.navigation.pane = Pane::Model;
    assert_eq!(
        app.ui.model.effort,
        Some(EffortLevel::Medium),
        "qwen3 supports effort -> the chain's medium"
    );
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(app.ui.model.effort, Some(EffortLevel::High));
    // Down to the model that speaks no effort dialect: no level at all.
    handle_input(&mut app, key(KeyCode::Down));
    assert!(
        app.ui.model.effort.is_none(),
        "a model without an effort dialect carries no level"
    );
    assert!(!app.ui.model.effort_touched, "not touched");
    // Up back to qwen3: the chain's value again, not the adjusted one.
    handle_input(&mut app, key(KeyCode::Up));
    assert_eq!(
        app.ui.model.effort,
        Some(EffortLevel::Medium),
        "back to qwen3 -> the chain's medium"
    );
}

/// Left/Right adjust the focused setting: the level wraps through
/// low/medium/high and the adjustment drops the default marker.
#[test]
fn test_model_pane_adjusts_effort() {
    let mut app = catalog_app(&[("qwen3.7-max", true, true)]);
    app.ui.navigation.pane = Pane::Model;
    handle_input(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.model.row, 1);
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(
        app.ui.model.effort,
        Some(EffortLevel::High),
        "Right cycles to high"
    );
    assert!(app.ui.model.effort_touched, "touched flag set");
    handle_input(&mut app, key(KeyCode::Left));
    assert_eq!(
        app.ui.model.effort,
        Some(EffortLevel::Medium),
        "Left cycles back to medium"
    );
    handle_input(&mut app, key(KeyCode::Left));
    assert_eq!(
        app.ui.model.effort,
        Some(EffortLevel::Low),
        "Left wraps to low"
    );
    assert!(app.ui.model.dirty, "the draft is modified");
}

/// Auto is a real stop in the cycle, reachable from both directions, so a
/// user who leaves the chain value can always get back to it.
#[test]
fn test_model_pane_cycles_auto() {
    let mut app = catalog_app(&[("qwen3.7-max", true, true)]);
    app.ui.navigation.pane = Pane::Model;
    assert_eq!(
        app.ui.model.effort,
        Some(EffortLevel::Medium),
        "the draft opens on the chain's medium"
    );
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(app.ui.model.effort, Some(EffortLevel::High));
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(app.ui.model.effort, None, "Right past high wraps to auto");
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(app.ui.model.effort, Some(EffortLevel::Low));
    handle_input(&mut app, key(KeyCode::Left));
    assert_eq!(
        app.ui.model.effort, None,
        "Left past low wraps back to auto"
    );
}

/// Left/Right is a no-op on a model that speaks no effort dialect, and Tab
/// cannot move the setting focus onto a setting the model does not support.
#[test]
fn test_model_pane_ignores_unsupported() {
    let mut app = catalog_app(&[("deepseek-chat", false, false)]);
    app.ui.navigation.pane = Pane::Model;
    handle_input(&mut app, key(KeyCode::Down));
    handle_input(&mut app, key(KeyCode::Right));
    assert!(
        app.ui.model.effort.is_none(),
        "no effort on an unsupported model"
    );
    assert!(!app.ui.model.effort_touched, "not touched");
    handle_input(&mut app, key(KeyCode::Tab));
    assert_eq!(
        app.ui.model.focus,
        ModelSettingFocus::Effort,
        "neither setting is supported, so the focus stays on effort"
    );
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Model,
        "Tab inside the pane never switches panes"
    );
}

/// Tab moves the setting focus between Reasoning Effort and Fast Mode when the
/// focused model supports both.
#[test]
fn test_model_pane_tab_focus() {
    let mut app = catalog_app(&[("qwen3.7-max", true, true)]);
    app.ui.navigation.pane = Pane::Model;
    handle_input(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.model.focus, ModelSettingFocus::Effort);
    handle_input(&mut app, key(KeyCode::Tab));
    assert_eq!(
        app.ui.model.focus,
        ModelSettingFocus::Fast,
        "Tab moves the focus to Fast"
    );
    handle_input(&mut app, key(KeyCode::BackTab));
    assert_eq!(
        app.ui.model.focus,
        ModelSettingFocus::Effort,
        "Shift+Tab moves it back"
    );
}

/// Opening /model via a typed slash input echoes the command exactly once, and
/// Esc-cancel (no change) must not add a second echo.
#[test]
fn test_model_open_echoes_once() {
    fn echoes(app: &App) -> usize {
        app.sessions
            .current
            .transcript
            .iter()
            .filter(|l| matches!(l, TranscriptLine::User(s) if s == "/model"))
            .count()
    }
    let mut app = catalog_app(&[("qwen3.7-max", true, true), ("other", true, true)]);
    app.ui.input.editor.set("/model".to_string());
    app.submit_input();
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Model,
        "the command opens the pane"
    );
    assert_eq!(echoes(&app), 1, "one /model echo on open");
    handle_input(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "Esc closes the pane"
    );
    assert_eq!(echoes(&app), 1, "Esc adds no second echo");
}

/// The /model pane Esc key discards the draft and closes back to the
/// transcript: nothing the user adjusted reaches the applied state.
#[test]
fn test_model_pane_esc_closes() {
    let mut app = catalog_app(&[("qwen3.7-max", true, true), ("other", true, true)]);
    app.ui.navigation.pane = Pane::Model;
    let applied = app
        .sessions
        .current
        .capabilities
        .model
        .catalog
        .applied
        .clone();
    handle_input(&mut app, key(KeyCode::Down));
    handle_input(&mut app, key(KeyCode::Right));
    handle_input(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.navigation.pane, Pane::Transcript);
    assert_eq!(
        app.sessions.current.capabilities.model.catalog.applied, applied,
        "Esc applies nothing"
    );
    assert_eq!(app.ui.model.row, 1, "the draft re-seeds on reopen");
    assert_eq!(
        app.ui.model.effort,
        Some(EffortLevel::Medium),
        "the adjusted level is discarded"
    );
}

/// Fast Mode is edited in the same pane as the model and the level: Tab takes
/// the arrows to the setting, the arrows move the tier, and Enter commits it
/// with the pick. No command reaches the tier, so it cannot be changed behind
/// the pane's back.
#[test]
fn test_picker_fast_inline() {
    let (mut app, events) = connected_app_with_events();
    let mut catalog = model_catalog(vec![model_entry(
        "glm-5.2",
        "Fable",
        model_caps(true, true),
    )]);
    // The chain resolves to a level so the draft opens on it, matching the
    // catalog fixtures: one Right takes focus Medium -> High.
    catalog.effort_level = Some(EffortLevel::Medium);
    app.refresh_model_catalog(catalog);
    app.ui.navigation.pane = Pane::Model;
    // The level is adjusted while the arrows are on Effort, so the commit has
    // all three dimensions to carry.
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(
        app.ui.model.effort,
        Some(EffortLevel::High),
        "Right adjusts the level the focus starts on"
    );
    handle_input(&mut app, key(KeyCode::Tab));
    assert_eq!(
        app.ui.model.focus,
        ModelSettingFocus::Fast,
        "Tab takes the arrows to the Fast setting"
    );
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(
        app.effective_model_speed(),
        SpeedMode::Fast,
        "Right turns the tier on"
    );
    assert!(
        SlashCommand::ALL.iter().all(|cmd| cmd.name() != "fast"),
        "the palette offers no tier command"
    );
    app.ui.input.editor.set("/fast".to_string());
    app.submit_input();
    assert_eq!(
        app.effective_model_speed(),
        SpeedMode::Fast,
        "a typed slash word does not move the tier"
    );
    handle_input(&mut app, key(KeyCode::Enter));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut shipped = String::new();
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(TransportEvent::Frame(frame)) if frame.contains("ModelSet") => {
                shipped = frame;
                break;
            }
            Ok(_) | Err(_) => {}
        }
    }
    assert!(
        shipped.contains("glm-5.2") && shipped.contains("high") && shipped.contains("\"fast\""),
        "one request carries the model, the level and the tier: {shipped}"
    );
}

/// A tier the focused model cannot serve is reported off before Enter: the
/// pane names the reason in place of the tier, and the draft commits
/// Standard rather than a tier the host would drop.
#[test]
fn test_picker_fast_refused_off() {
    let mut app = catalog_app(&[("glm-5.2", true, true), ("plain-model", true, false)]);
    app.ui.navigation.pane = Pane::Model;
    handle_input(&mut app, key(KeyCode::Tab));
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(
        app.effective_model_speed(),
        SpeedMode::Fast,
        "the focused model serves the tier"
    );
    handle_input(&mut app, key(KeyCode::Down));
    let fast_available = app
        .sessions
        .current
        .capabilities
        .model
        .fast_available_at(app.ui.model.row);
    assert!(
        app.ui.model.fast_forced_off(fast_available),
        "the new focus cannot serve the tier the draft holds"
    );
    assert_eq!(
        app.effective_model_speed(),
        SpeedMode::Standard,
        "the draft does not ask for a tier this model cannot serve"
    );
    let out = render_text(&app, 84, 24);
    assert!(
        out.contains("off (required by selected model)"),
        "the pane says why the tier is off: {out}"
    );
    // The narrow layout renders the same line from another chunk index, so the
    // reason has to survive that path too.
    let narrow = render_text(&app, 64, 24);
    assert!(
        narrow.contains("off (required by selected model)"),
        "the reason shows in the narrow layout as well: {narrow}"
    );
}

/// The /skills pane Esc key closes back to the transcript, through the real
/// entry point and while an agent runs. Calling the inner input handler
/// skipped the abort arm that Esc meets first, so the test passed while the
/// key was in fact being taken to interrupt the running agent.
#[test]
fn test_skills_pane_esc_closes() {
    let mut app = working_app();
    app.start_run_for_test(0);
    app.ui.navigation.pane = Pane::Skills;
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.ui.navigation.pane, Pane::Transcript);
    assert!(
        !app.cancelling(),
        "closing a pane must not interrupt the running agent"
    );
}

/// Up/Down moves the skill cursor within the list bounds.
#[test]
fn test_skills_pane_nav() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Skills;
    app.sessions.current.capabilities.skills.entries = vec![
        SkillEntry {
            name: "alpha".into(),
            description: "a".into(),
            origin: "user".into(),
            invocable: true,
            body_token_estimate: 100,
            user_invocable: true,
            usage: None,
        },
        SkillEntry {
            name: "beta".into(),
            description: "b".into(),
            origin: "user".into(),
            invocable: true,
            body_token_estimate: 200,
            user_invocable: true,
            usage: None,
        },
    ];
    assert_eq!(app.ui.skills_pane.cursor(), 0);
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(app.ui.skills_pane.cursor(), 1, "Down moves cursor to 1");
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.skills_pane.cursor(), 0, "Up moves cursor back to 0");
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.skills_pane.cursor(), 0, "Up saturates at 0");
}

/// Enter opens the detail view; Esc returns to the list. Without a session
/// the body cannot be requested, so the detail opens with it unavailable
/// rather than staying on a request that was never sent.
#[test]
fn test_skills_pane_detail() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Skills;
    app.sessions.current.capabilities.skills.entries = vec![SkillEntry {
        name: "alpha".into(),
        description: "a".into(),
        origin: "user".into(),
        invocable: true,
        body_token_estimate: 100,
        user_invocable: true,
        usage: None,
    }];
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        app.ui.skills_pane.detail().is_some(),
        "Enter opens the detail"
    );
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(
        app.ui.skills_pane.detail().is_none(),
        "Esc returns to the list"
    );
}

/// The t key toggles a skill disabled in the detail view.
#[test]
fn test_skills_pane_toggle() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Skills;
    app.sessions.current.capabilities.skills.entries = vec![SkillEntry {
        name: "alpha".into(),
        description: "a".into(),
        origin: "user".into(),
        invocable: true,
        body_token_estimate: 100,
        user_invocable: true,
        usage: None,
    }];
    app.ui.skills_pane.open_unavailable("alpha".into());
    assert!(
        app.sessions.current.capabilities.skills.disabled.is_empty(),
        "starts clean"
    );
    handle_working(&mut app, key(KeyCode::Char('t')));
    assert!(
        app.sessions
            .current
            .capabilities
            .skills
            .disabled
            .contains("alpha"),
        "t disables alpha"
    );
    handle_working(&mut app, key(KeyCode::Char('t')));
    assert!(
        !app.sessions
            .current
            .capabilities
            .skills
            .disabled
            .contains("alpha"),
        "t re-enables alpha"
    );
}

/// The @ key opens the skill picker from an empty input box and resets the
/// cursor to the top.
#[test]
fn test_skill_picker_opens() {
    let mut app = working_app();
    app.sessions.current.capabilities.skills.entries = vec![SkillEntry {
        name: "alpha".into(),
        description: "a".into(),
        origin: "user".into(),
        invocable: true,
        body_token_estimate: 100,
        user_invocable: true,
        usage: None,
    }];
    assert!(!app.ui.navigation.skill_picker.open);
    handle_working(&mut app, key(KeyCode::Char('@')));
    assert!(app.ui.navigation.skill_picker.open, "@ opens the picker");
    assert_eq!(
        app.ui.navigation.skill_picker.sel.get(),
        0,
        "cursor resets to 0"
    );
    assert!(
        app.ui.input.editor.value().contains('@'),
        "@ pushed into input so free-form typing survives"
    );
}

/// The @ key does nothing when the input box is not empty.
#[test]
fn test_skill_picker_blocked() {
    let mut app = working_app();
    app.ui.input.editor.push('x');
    handle_working(&mut app, key(KeyCode::Char('@')));
    assert!(
        !app.ui.navigation.skill_picker.open,
        "@ must not open when input non-empty"
    );
    assert!(
        app.ui.input.editor.value().contains('@'),
        "@ landed in the input box instead"
    );
}

/// Up/Down navigate the picker list.
#[test]
fn test_skill_picker_nav() {
    let mut app = working_app();
    app.sessions.current.capabilities.skills.entries = vec![
        SkillEntry {
            name: "alpha".into(),
            description: "a".into(),
            origin: "user".into(),
            invocable: true,
            body_token_estimate: 100,
            user_invocable: true,
            usage: None,
        },
        SkillEntry {
            name: "beta".into(),
            description: "b".into(),
            origin: "user".into(),
            invocable: true,
            body_token_estimate: 200,
            user_invocable: true,
            usage: None,
        },
    ];
    handle_working(&mut app, key(KeyCode::Char('@')));
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(
        app.ui.navigation.skill_picker.sel.get(),
        1,
        "Down moves picker cursor"
    );
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.ui.navigation.skill_picker.sel.get(), 0, "Up moves back");
}

/// Esc closes the picker without inserting anything.
#[test]
fn test_skill_picker_esc() {
    let mut app = working_app();
    app.sessions.current.capabilities.skills.entries = vec![SkillEntry {
        name: "alpha".into(),
        description: "a".into(),
        origin: "user".into(),
        invocable: true,
        body_token_estimate: 100,
        user_invocable: true,
        usage: None,
    }];
    handle_working(&mut app, key(KeyCode::Char('@')));
    assert!(app.ui.navigation.skill_picker.open);
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(
        !app.ui.navigation.skill_picker.open,
        "Esc closes the picker"
    );
    assert!(app.ui.input.editor.is_empty(), "no text inserted on Esc");
}

/// Enter inserts the selected skill into the input box (does not auto-submit,
/// so the user can append args before pressing Enter again).
#[test]
fn test_skill_picker_enter_submit() {
    let mut app = working_app();
    app.sessions.current.capabilities.skills.entries = vec![SkillEntry {
        name: "alpha".into(),
        description: "a".into(),
        origin: "user".into(),
        invocable: true,
        body_token_estimate: 100,
        user_invocable: true,
        usage: None,
    }];
    handle_working(&mut app, key(KeyCode::Char('@')));
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        !app.ui.navigation.skill_picker.open,
        "Enter closes the picker"
    );
    assert!(
        app.ui.input.editor.value().contains("@skill:alpha"),
        "@skill:alpha inserted into the input box"
    );
    assert!(
        !app.ui.input.editor.value().is_empty(),
        "input holds the text for a second Enter to submit"
    );
}

/// Enter with no skills closes the picker. The lone @ from the trigger
/// stays in the input (no skill text to replace it with).
#[test]
fn test_skill_picker_enter_empty() {
    let mut app = working_app();
    handle_working(&mut app, key(KeyCode::Char('@')));
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        !app.ui.navigation.skill_picker.open,
        "Enter closes on empty list"
    );
    assert!(
        app.ui.input.editor.value().contains('@'),
        "lone @ stays in input when no skill selected"
    );
}

/// A typed char while the picker is open closes the picker and lands in the
/// input box (no silent char loss). The modal falls through after closing.
#[test]
fn test_skill_picker_char() {
    let mut app = working_app();
    app.sessions.current.capabilities.skills.entries = vec![SkillEntry {
        name: "alpha".into(),
        description: "a".into(),
        origin: "user".into(),
        invocable: true,
        body_token_estimate: 100,
        user_invocable: true,
        usage: None,
    }];
    handle_working(&mut app, key(KeyCode::Char('@')));
    assert!(app.ui.navigation.skill_picker.open);
    handle_working(&mut app, key(KeyCode::Char('x')));
    assert!(
        app.ui.navigation.skill_picker.open,
        "picker stays open for filter typing"
    );
    assert!(
        app.ui.input.editor.value().contains('x'),
        "char lands in input as filter"
    );
}

/// Backspace on a lone @ closes the picker + clears the input.
#[test]
fn test_skill_picker_backspace_close() {
    let mut app = working_app();
    app.sessions.current.capabilities.skills.entries = vec![SkillEntry {
        name: "alpha".into(),
        description: "a".into(),
        origin: "user".into(),
        invocable: true,
        body_token_estimate: 100,
        user_invocable: true,
        usage: None,
    }];
    handle_working(&mut app, key(KeyCode::Char('@')));
    handle_working(&mut app, key(KeyCode::Backspace));
    assert!(
        !app.ui.navigation.skill_picker.open,
        "backspace on lone @ closes picker"
    );
    assert!(app.ui.input.editor.is_empty(), "input cleared");
}

/// A colon signals a namespace prefix — closes the picker for free-form.
#[test]
fn test_skill_picker_colon_closes() {
    let mut app = working_app();
    app.sessions.current.capabilities.skills.entries = vec![SkillEntry {
        name: "alpha".into(),
        description: "a".into(),
        origin: "user".into(),
        invocable: true,
        body_token_estimate: 100,
        user_invocable: true,
        usage: None,
    }];
    handle_working(&mut app, key(KeyCode::Char('@')));
    handle_working(&mut app, key(KeyCode::Char('s')));
    handle_working(&mut app, key(KeyCode::Char(':')));
    assert!(
        !app.ui.navigation.skill_picker.open,
        "colon closes picker for free-form"
    );
}

/// Filter narrows the list; Down+Enter selects from the filtered list
/// (not the unfiltered list — prevents cursor/index desync).
#[test]
fn test_skill_picker_filter_select() {
    let mut app = working_app();
    app.sessions.current.capabilities.skills.entries = vec![
        SkillEntry {
            name: "alpha".into(),
            description: "a".into(),
            origin: "user".into(),
            invocable: true,
            body_token_estimate: 100,
            user_invocable: true,
            usage: None,
        },
        SkillEntry {
            name: "beta".into(),
            description: "b".into(),
            origin: "user".into(),
            invocable: true,
            body_token_estimate: 200,
            user_invocable: true,
            usage: None,
        },
    ];
    handle_working(&mut app, key(KeyCode::Char('@')));
    handle_working(&mut app, key(KeyCode::Char('b')));
    // Filter narrows to just "beta". Enter should select beta.
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(!app.ui.navigation.skill_picker.open, "Enter closes picker");
    assert!(
        app.ui.input.editor.value().contains("@skill:beta"),
        "filtered selection inserts beta: {}",
        app.ui.input.editor.value()
    );
    assert!(
        !app.ui.input.editor.value().contains("alpha"),
        "alpha not selected: {}",
        app.ui.input.editor.value()
    );
}

/// Typing keys do not reach the input box while a pane stands in for it. The
/// box is off screen, so a character pushed into it is invisible -- and the
/// resulting non-empty input silently disables the pane's own Enter, which
/// guards on an empty box. Both panes that were missing from the typing lists
/// are pinned here.
#[test]
fn test_pane_swallows_typing() {
    for pane in [Pane::Agents, Pane::Skills] {
        let mut app = working_app();
        app.ui.navigation.pane = pane;
        handle_working(&mut app, key(KeyCode::Char('x')));
        assert!(
            app.ui.input.editor.is_empty(),
            "{pane:?}: a character must not enter the hidden input box"
        );
        handle_working(&mut app, key(KeyCode::Char('/')));
        assert!(
            !app.ui.navigation.palette.open,
            "{pane:?}: slash does not open the palette over a pane"
        );
        app.ui.input.editor.push('a');
        handle_working(&mut app, key(KeyCode::Backspace));
        assert_eq!(
            app.ui.input.editor.value(),
            "a",
            "{pane:?}: backspace does not edit the hidden box"
        );
    }
}

/// Pasting routes to the palette query when the palette is open (the
/// hint-after-space arg surface), else to the input box. Guards the bug where a
/// pasted file path / sid went to the input bar and the popup never saw it.
#[test]
fn test_paste_routes_palette_open() {
    // Palette open: paste lands in the query (the arg surface).
    let mut app = working_app();
    app.open_palette();
    app.ui.navigation.palette.query = "resume ".into();
    app.apply_paste_token("/tmp/export.json");
    assert_eq!(
        app.ui.navigation.palette.query, "resume /tmp/export.json",
        "palette paste should append to the query"
    );
    assert_eq!(
        app.ui.input.editor.value(),
        "",
        "input must stay empty when palette open"
    );

    // Palette closed: paste lands in the input box.
    let mut app = working_app();
    app.apply_paste_token("hello");
    let text = app.ui.input.editor.value();
    assert_eq!(text, "hello", "input paste appends to the box");
}

/// The /status pane Left / Right keys cycle the sub-tab (Status → Config →
/// Usage → back to Status).
#[test]
fn test_status_pane_cycles_subtab() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Status;
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(app.ui.navigation.status_pane.tab, StatusTab::Config);
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(app.ui.navigation.status_pane.tab, StatusTab::Usage);
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(app.ui.navigation.status_pane.tab, StatusTab::Status);
}

#[test]
fn test_login_local_skips_console() {
    let mut app = composition::app();
    handle_login(&mut app, key(KeyCode::Char('3')));
    assert_eq!(app.ui.navigation.screen, Screen::Working);
    assert_eq!(app.ui.navigation.login_mode, Some(LoginMode::Local));
}

#[test]
fn test_login_lands_clean() {
    // Landing on Working leaves the transcript empty (placeholder hint only).
    let mut app = composition::app();
    app.sessions.current.transcript.reset();
    handle_login(&mut app, key(KeyCode::Char('1')));
    assert_eq!(app.ui.navigation.screen, Screen::Working);
    assert_eq!(app.sessions.current.transcript.len(), 0);
    // landing again on an already-alive transcript does not seed anything
    let before = app.sessions.current.transcript.len();
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::System("x".into()));
    handle_login(&mut app, key(KeyCode::Char('1')));
    assert_eq!(app.sessions.current.transcript.len(), before + 1);
}

#[test]
fn test_palette_enter_runs_selected() {
    let mut app = working_app();
    app.open_palette();
    handle_palette(&mut app, key(KeyCode::Enter));
    assert!(!app.ui.navigation.palette.open);
}

#[test]
fn test_palette_select_runs_command() {
    // Selecting a command routes through the input box and the unified
    // submit path (echoed as a User turn, then runs). Auto-run on select.
    let mut app = working_app();
    app.open_palette();
    for c in "context".chars() {
        handle_palette(&mut app, key(KeyCode::Char(c)));
    }
    assert_eq!(app.selected_command().map(|c| c.name()), Some("/context"));
    handle_palette(&mut app, key(KeyCode::Enter));
    assert!(!app.ui.navigation.palette.open);
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "/context")),
        "palette select must echo the command as a User turn"
    );
    assert!(
        matches!(
            app.sessions.current.transcript.last(),
            Some(TranscriptLine::ContextGrid(_))
        ),
        "the /context response should follow the echoed command"
    );
}

#[test]
fn test_palette_accepts_space_separator() {
    // A space in the palette query is the arg separator for arg-taking slash
    // commands (/permissions git off). Without accepting it the query arrives
    // concatenated (permissionsgitoff) + arg commands are unreachable. The
    // space lands in the query so the raw-submit branch can ship the typed
    // command as a slash when no palette entry matches.
    let mut app = working_app();
    app.open_palette();
    for c in "permissions git".chars() {
        handle_palette(&mut app, key(KeyCode::Char(c)));
    }
    assert!(
        app.ui.navigation.palette.query.contains(' '),
        "space must land in the query, not be dropped: {:?}",
        app.ui.navigation.palette.query
    );
}

#[test]
fn test_palette_tui_local_via() {
    // /debug is a palette-registered local command: typing "debug" filters
    // the list to it, Enter selects + auto-runs it through the unified
    // submit path (echoed as a User turn + dispatched).
    let mut app = working_app();
    app.open_palette();
    for c in "debug".chars() {
        handle_palette(&mut app, key(KeyCode::Char(c)));
    }
    handle_palette(&mut app, key(KeyCode::Enter));
    assert!(!app.ui.navigation.palette.open);
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "/debug")),
        "palette select of /debug must echo as a User turn"
    );
    assert!(
        matches!(app.sessions.current.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("debug")),
        "the debug response should follow the echo"
    );
    // A leading slash in the palette query is normalized, not doubled.
    let mut app = working_app();
    app.open_palette();
    for c in "/debug".chars() {
        handle_palette(&mut app, key(KeyCode::Char(c)));
    }
    handle_palette(&mut app, key(KeyCode::Enter));
    assert!(
        matches!(app.sessions.current.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("debug")),
        "leading slash in palette query must be normalized, not double-slashed"
    );
}

/// Selecting an arg-taking command (/search) keeps the palette OPEN and seeds
/// the query with the name + a trailing space — the user keeps typing the
/// argument in the popup (which shows the arg hint after the space), then
/// presses Enter to submit. Argless commands auto-run on select.
#[test]
fn test_palette_arg_keeps_open() {
    let mut app = working_app();
    app.open_palette();
    for c in "search".chars() {
        handle_palette(&mut app, key(KeyCode::Char(c)));
    }
    assert_eq!(
        app.selected_command().map(|c| c.name()),
        Some("/search"),
        "/search must be palette-discoverable"
    );
    handle_palette(&mut app, key(KeyCode::Enter));
    assert!(
        app.ui.navigation.palette.open,
        "palette stays open for the arg"
    );
    assert_eq!(
        app.ui.navigation.palette.query, "search ",
        "arg command seeds the query with name + space"
    );
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .all(|l| !matches!(l, TranscriptLine::User(_))),
        "arg command must NOT auto-submit (waits for the query)"
    );
}

#[test]
fn test_palette_typing_filters() {
    let mut app = working_app();
    app.open_palette();
    assert_eq!(app.palette_len(), SlashCommand::ALL.len());
    handle_palette(&mut app, key(KeyCode::Char('s')));
    handle_palette(&mut app, key(KeyCode::Char('p')));
    handle_palette(&mut app, key(KeyCode::Char('e')));
    assert_eq!(app.ui.navigation.palette.sel, 0);
    let cmd = app.selected_command().expect("filtered list non-empty");
    assert!(cmd.name().contains("spe"));
    assert!(app.palette_len() < SlashCommand::ALL.len());
}

#[test]
fn test_palette_backspace_edits() {
    let mut app = working_app();
    app.open_palette();
    handle_palette(&mut app, key(KeyCode::Char('x')));
    assert!(!app.ui.navigation.palette.query.is_empty());
    handle_palette(&mut app, key(KeyCode::Backspace));
    assert!(app.ui.navigation.palette.query.is_empty());
    assert!(app.ui.navigation.palette.open);
    handle_palette(&mut app, key(KeyCode::Backspace));
    assert!(!app.ui.navigation.palette.open);
}

#[test]
fn test_tab_cycles_pane() {
    let mut app = working_app();
    assert_eq!(app.ui.navigation.pane, Pane::Transcript);
    cycle_pane(&mut app);
    assert_eq!(app.ui.navigation.pane, Pane::Spec);
}

#[test]
fn test_diff_approve_key_sets() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Diff;
    app.workflow.stage = Stage::Implementing;
    app.ui.input.editor.clear();
    handle_working(&mut app, key(KeyCode::Down));
    handle_working(&mut app, key(KeyCode::Char('a')));
    assert_eq!(app.workflow.diff.hunks[1].approved, Verdict::Approved);
}

#[test]
fn test_diff_reject_key_sets() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Diff;
    app.workflow.stage = Stage::Implementing;
    app.ui.input.editor.clear();
    handle_working(&mut app, key(KeyCode::Char('r')));
    assert_eq!(
        app.workflow.diff.current().unwrap().approved,
        Verdict::Rejected
    );
}

/// The 'i' key on the Review pane in Verify stage routes to rework_in_pane.
/// 'r' is caught first by the reject arm (both rejectable and reworkable hold
/// on Review+Verify), so 'i' is the unique path to the rework arm. With no
/// finding under the cursor, rework is a no-op (no panic); the arm still fires.
#[test]
fn test_review_rework_key_routes() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Review;
    app.workflow.stage = Stage::Verify;
    app.ui.input.editor.clear();
    handle_working(&mut app, key(KeyCode::Char('i')));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Review,
        "no finding -> rework is a no-op"
    );
    assert!(
        app.ui.input.editor.is_empty(),
        "i consumed by the rework arm, not pushed to the input box"
    );
}

/// Repeated Esc while a run is in flight remains an idempotent abort and
/// leaves queued input intact throughout the cancellation round trip.
#[test]
fn test_repeated_esc_keeps() {
    let mut app = working_app();
    attach_connection(&mut app);
    app.start_run_for_test(0);
    app.ui.input.queue.push(InputItem::Message("task a".into()));
    handle_working(&mut app, key(KeyCode::Esc));
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(
        app.ui
            .input
            .queue
            .iter()
            .any(|p| matches!(p, InputItem::Message(t) if t == "task a")),
        "repeated Esc keeps the queued message"
    );
    let empty = app.ui.input.editor.is_empty();
    assert!(empty, "Esc does not touch the input box");
    assert!(app.cancelling(), "Esc leaves cancellation in flight");
}

/// Explicit queue recall merges a queued message with a half-typed draft
/// instead of overwriting it.
#[test]
fn test_recall_merges_draft() {
    let mut app = working_app();
    attach_connection(&mut app);
    app.start_run_for_test(0);
    app.ui.input.queue.push(InputItem::Message("task a".into()));
    app.ui.input.editor.set("half draft".into());
    app.ui.navigation.pane = Pane::Queue;
    handle_working(&mut app, key(KeyCode::Char('R')));
    assert_eq!(
        app.ui.input.editor.value(),
        "task a\nhalf draft",
        "explicit recall prepends the message without overwriting the draft"
    );
}

/// During the cancelling window, Esc on a pane with its own close must close
/// the pane without changing the queued input.
#[test]
fn test_esc_cancelling_closes_pane() {
    let mut app = working_app();
    attach_connection(&mut app);
    app.start_run_for_test(0);
    app.ui.input.queue.push(InputItem::Message("task a".into()));
    // Esc1: interrupt (cancelling, agent_busy still true, queue intact).
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(app.cancelling());
    // Open a pane with its own Esc, then Esc: the pane closes, the queue
    // is NOT popped (the pane-close key is not stolen for a recall).
    app.ui.navigation.pane = Pane::Memory;
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "Esc closes the Memory pane"
    );
    assert!(
        !app.ui.input.queue.is_empty(),
        "the queue is not popped while closing the pane"
    );
    assert!(
        app.ui.input.editor.is_empty(),
        "input stays empty -- no recall on the pane-close Esc"
    );
}

/// Same gate for /trajectory, which owns multi-level Esc (level 2 -> 1 -> 0
/// -> close). The cancelling window must not steal its Esc for a queue pop.
/// Pins that pane_owns_esc covers Trajectory, the pane the copied-list bug
/// left in neither Esc arm.
#[test]
fn test_esc_cancelling_backs_trajectory() {
    let mut app = working_app();
    attach_connection(&mut app);
    app.start_run_for_test(0);
    app.ui.input.queue.push(InputItem::Message("task a".into()));
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(app.cancelling());
    app.ui.navigation.pane = Pane::Trajectory;
    app.ui.trajectory.set_level(1);
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.trajectory.level(),
        0,
        "Esc backs the trajectory pane a level, not pops the queue"
    );
    assert!(
        !app.ui.input.queue.is_empty(),
        "the queue is not popped while backing the pane"
    );
}

#[test]
fn test_console_signoff_appends_audit() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Console;
    let before = app.workflow.review.audit_trail.len();
    handle_console(&mut app, key(KeyCode::Char('a')));
    assert_eq!(app.workflow.review.audit_trail.len(), before + 1);
}

#[test]
fn test_console_reject_writes_org() {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Console;
    handle_console(&mut app, key(KeyCode::Char('r')));
    assert!(app.workflow.review.findings[app.workflow.review.focus].resolved());
    assert!(matches!(app.sessions.current.transcript.last(),
            Some(TranscriptLine::System(s)) if s.contains("org eval")));
}

#[test]
fn test_approval_enter_clears_popup() {
    // The verdict is transient (a dismissable card, not a transcript row), so
    // Enter must clear the popup without logging a verdict line.
    let mut app = working_app();
    app.sessions.current.prompt = Some(PendingPrompt::approval_card(
        RequestId(0),
        Approval {
            tool: "FsWrite".to_string(),
            args: "".to_string(),
            reason: "".to_string(),
            selected: 1,
            call_id: String::new(),
            options: Vec::new(),
            ..Default::default()
        },
    ));
    handle_approval(&mut app, key(KeyCode::Enter));
    assert!(app.approval().is_none(), "popup must clear on confirm");
}

// --- Approval key-dispatch tests through handle_working ---
// These drive the full key dispatch chain (handle_working → handle_input →
// handle_generic_input → handle_approval), not just handle_approval directly.
// They verify key→action mappings: navigation moves the cursor, Enter produces
// the correct verdict, and always-allow persists a permission rule.

fn approval_app(selected: usize) -> App {
    let mut app = working_app();
    app.sessions.current.prompt = Some(PendingPrompt::approval_card(
        RequestId(0),
        Approval {
            tool: "bash".to_string(),
            args: r#"{"command":"ls"}"#.to_string(),
            reason: "test".to_string(),
            selected,
            call_id: String::new(),
            options: Vec::new(),
            ..Default::default()
        },
    ));
    app
}

#[test]
fn test_approval_down_dont_ask() {
    // Down from Yes(0) goes to Yes-don't-ask(2) in display order. Full dispatch.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(
        app.approval().expect("approval").selected,
        2,
        "Down must move cursor to Yes-don't-ask"
    );
}

#[test]
fn test_approval_up_wraps() {
    // Up from Yes(0) wraps to No(1) in display order. Full dispatch.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(
        app.approval().expect("approval").selected,
        1,
        "Up must wrap cursor to No"
    );
}

#[test]
fn test_approval_down_wraps_yes() {
    // Down from No(1) wraps to Yes(0) in display order. Full dispatch.
    let mut app = approval_app(1);
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(
        app.approval().expect("approval").selected,
        0,
        "Down must wrap from No to Yes"
    );
}

#[test]
fn test_approval_left_navigates_previous() {
    // Left from No(1) goes to Yes-don't-ask(2) in display order. Full dispatch.
    let mut app = approval_app(1);
    handle_working(&mut app, key(KeyCode::Left));
    assert_eq!(
        app.approval().expect("approval").selected,
        2,
        "Left must move cursor to Yes-don't-ask"
    );
}

#[test]
fn test_approval_right_navigates_next() {
    // Right from Yes-don't-ask(2) goes to No(1) in display order. Full dispatch.
    let mut app = approval_app(2);
    handle_working(&mut app, key(KeyCode::Right));
    assert_eq!(
        app.approval().expect("approval").selected,
        1,
        "Right must move cursor to No"
    );
}

#[test]
fn test_approval_key2_dont_ask() {
    // Pressing '2' directly selects Yes-don't-ask. Full dispatch.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Char('2')));
    assert_eq!(
        app.approval().expect("approval").selected,
        2,
        "'2' must select Yes-don't-ask"
    );
}

#[test]
fn test_key3_selects_no_option() {
    // Pressing '3' directly selects No. Full dispatch.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Char('3')));
    assert_eq!(
        app.approval().expect("approval").selected,
        1,
        "'3' must select No"
    );
}

#[test]
fn test_approval_enter_yes_approve() {
    // Enter on Yes(0) → approve verdict. Full dispatch through handle_working.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        app.approval().is_none(),
        "approval should clear after confirm"
    );
}

#[test]
fn test_approval_enter_no_reject() {
    // Enter on No(1) → reject verdict. Full dispatch through handle_working.
    let mut app = approval_app(1);
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        app.approval().is_none(),
        "approval should clear after confirm"
    );
}

#[test]
fn test_approval_nav_confirm_verdict() {
    // Navigate to No via Up (Yes wraps to No in display order), then Enter
    // clears. This catches the confirm=reject bug: if navigation works but
    // confirm reads the wrong cursor, the verdict will mismatch. Full dispatch.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.approval().unwrap().selected, 1);
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        app.approval().is_none(),
        "Up+Enter must clear the popup: {:?}",
        app.sessions.current.prompt
    );
}

#[test]
fn test_approval_nav_full_cycle() {
    // Full cycle: Down Down Down returns to the same option.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Down)); // 0→2
    handle_working(&mut app, key(KeyCode::Down)); // 2→1
    handle_working(&mut app, key(KeyCode::Down)); // 1→0 (wrap)
    assert_eq!(
        app.approval().unwrap().selected,
        0,
        "three Downs must wrap back to Yes"
    );
}

#[test]
fn test_esc_keeps_input() {
    let mut app = working_app();
    app.ui.input.editor.set("draft".into());
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(!app.ui.quit, "idle Esc must not quit");
    let text = app.ui.input.editor.value();
    assert_eq!(text, "draft", "Ctrl+U owns input clearing");
}

/// While a run is in flight, Esc interrupts and leaves the draft untouched.
/// The prior behavior cleared the draft first (then aborted on a second
/// Esc), which destroyed the user's half-typed text and forced a two-press
/// interrupt. One Esc now interrupts; the draft survives for the user to
/// edit or clear deliberately.
#[test]
fn test_busy_esc_keeps_draft() {
    let mut app = working_app();
    attach_connection(&mut app);
    app.start_run_for_test(0);
    app.ui.input.editor.set("draft".into());
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(app.cancelling(), "busy Esc interrupts the run");
    let text = app.ui.input.editor.value();
    assert_eq!(text, "draft", "the draft is left untouched");
}

/// While a run is in flight, Esc in the /worktrees pane closes the pane back
/// to the transcript rather than aborting the run — the sibling /memory pane
/// does the same, and /worktrees must match so a user dismissing the pane
/// does not lose the in-flight turn. Without Pane::Worktree in the busy-Esc
/// exclusion the abort arm shadowed the pane's Esc-close, so Esc always
/// aborted and the pane could not be dismissed while busy.
#[test]
fn test_worktree_esc_closes_busy() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Worktree;
    app.start_run_for_test(0);
    app.ui.input.editor.clear();
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "Esc closes the worktree pane"
    );
    assert!(
        !app.cancelling(),
        "must not abort the run when dismissing the worktree pane"
    );
}

fn shift_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

fn fleet_app(n: usize) -> App {
    let mut app = composition::app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.viewport = ViewportMode::Working;
    for i in 0..n {
        app.sessions
            .current
            .capabilities
            .fleet
            .entries
            .push(FleetEntry {
                agent_id: format!("child-{i}"),
                subagent_type: "explore".into(),
                turn: 1,
                tokens: 10,
                tool_uses: 0,
                last_activity: None,
                completed: None,
                completed_at: None,
                started_at: None,
            });
    }
    app
}

/// Shift+Down from no selection snaps to the top row (not past it); further
/// presses advance; Shift+Up clamps at the top without wrapping.
#[test]
fn test_shift_arrow_moves_fleet() {
    let mut app = fleet_app(3);
    assert!(app.ui.fleet.selected().is_none());
    handle_working(&mut app, shift_key(KeyCode::Down));
    assert_eq!(
        app.ui.fleet.selected(),
        Some(0),
        "first press snaps to the top row"
    );
    handle_working(&mut app, shift_key(KeyCode::Down));
    assert_eq!(app.ui.fleet.selected(), Some(1));
    handle_working(&mut app, shift_key(KeyCode::Up));
    assert_eq!(app.ui.fleet.selected(), Some(0));
    handle_working(&mut app, shift_key(KeyCode::Up));
    assert_eq!(app.ui.fleet.selected(), Some(0), "clamps at top, no wrap");
    // A Shift+non-arrow key is not consumed: selection stays put.
    handle_working(&mut app, shift_key(KeyCode::Char('x')));
    assert_eq!(app.ui.fleet.selected(), Some(0));
}

/// Shift+Up/Down exits a drilled-in teammate view back to the fleet,
/// regardless of whether the viewed child is still running. A per-turn Esc
/// cancel does not stop the run, so a child that never idles cannot trap the
/// user behind the view. Both arrow directions exit.
#[test]
fn test_shift_arrow_exits_teammate() {
    let mut app = fleet_app(1);
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "child-0".into(),
        ..Default::default()
    });
    handle_working(&mut app, shift_key(KeyCode::Down));
    assert!(
        app.ui.teammate_view.is_none(),
        "Shift+Down exits the teammate view even while the child runs"
    );
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "child-0".into(),
        ..Default::default()
    });
    handle_working(&mut app, shift_key(KeyCode::Up));
    assert!(
        app.ui.teammate_view.is_none(),
        "Shift+Up also exits the teammate view"
    );
    assert!(
        app.sessions.current.capabilities.fleet.entries[0]
            .completed
            .is_none(),
        "the child keeps running after the view exits"
    );
}

/// Esc in the teammate view never leaks to the parent, even with a draft
/// in the input box and a parent run in flight. The old input.is_empty()
/// guard let Esc fall through to abort_run on the parent; the teammate arm
/// now consumes Esc unconditionally (interrupt the viewed child, or a
/// no-op toast when idle) so the parent run is never interrupted.
#[test]
fn test_teammate_esc_no_leak() {
    let mut app = fleet_app(1);
    app.start_run_for_test(0);
    app.ui.input.editor.set("a draft steering note".into());
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "child-0".into(),
        ..Default::default()
    });
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(
        !app.cancelling(),
        "Esc in the teammate view must not interrupt the parent run"
    );
    assert!(
        app.ui.teammate_view.is_some(),
        "Esc keeps the view open (exit is on Shift+Up/Down)"
    );
    assert!(
        !app.ui.input.editor.is_empty(),
        "Esc does not clear the draft (Ctrl+U owns that)"
    );
}

/// A pane that owns Esc closes before the teammate Esc arm fires, so the
/// child is not interrupted while the user manages a pane.
#[test]
fn test_teammate_esc_pane_first() {
    let mut app = fleet_app(1);
    app.start_run_for_test(0);
    app.ui.navigation.pane = Pane::Agents;
    app.ui.teammate_view = Some(TeammateView {
        child_sid: "child-0".into(),
        ..Default::default()
    });
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "pane closes first"
    );
    assert!(app.ui.teammate_view.is_some(), "view stays open");
    assert!(!app.cancelling(), "child not interrupted");
}

/// Rendering a populated fleet paints one strip row per child, each carrying
/// the type + the verb inferred from its last tool.
#[test]
fn test_fleet_strip_renders_rows() {
    let app = fleet_app(2);
    let text = render_text(&app, 80, 24);
    assert!(text.contains("explore"), "strip shows the child type");
    assert!(
        text.contains("searching") || text.contains("thinking"),
        "verb rendered"
    );
}

/// Empty-input Enter on a selected fleet row drills into the child's
/// teammate view via the agent id, not the transcript cursor.
#[test]
fn test_enter_fleet_drills_teammate() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.viewport = ViewportMode::Working;
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::Subagent {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            summary: "found auth".into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "explore".into(),
            turn: 1,
            tokens: 10,
            tool_uses: 0,
            last_activity: None,
            completed: None,
            completed_at: None,
            started_at: None,
        });
    app.ui.fleet.set_selected(Some(0));
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        app.ui.teammate_view.is_some(),
        "Enter on the selected fleet row opens the teammate view"
    );
    assert_eq!(
        app.ui.teammate_view.as_ref().unwrap().child_sid,
        "c1",
        "view targets the fleet agent id"
    );
}

/// With no cursor, Ctrl+O expands whichever block the transcript holds
/// latest — here the reasoning summary that arrived after the delegation.
/// A single delegation anywhere used to swallow the key for every kind.
#[test]
fn test_ctrl_o_expands_latest() {
    let mut app = working_app();
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::Subagent {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            summary: "found auth".into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::ThoughtFor {
            ms: Some(19_000),
            reasoning: Some("a long train of reasoning".into()),
            tool_summary: None,
            turn_id: "t1".into(),
        });
    assert!(app.ui.selection.transcript.anchor.is_none(), "no cursor");
    handle_ctrl_o(&mut app);
    assert!(
        app.ui
            .transcript_view
            .expanded
            .thinking
            .contains(&"t1".into()),
        "the later block expands: {:?}",
        app.ui.transcript_view.expanded.thinking
    );
    assert!(
        app.ui.transcript_view.expanded.subagents.is_empty(),
        "the earlier delegation is untouched: {:?}",
        app.ui.transcript_view.expanded.subagents
    );
    handle_ctrl_o(&mut app);
    assert!(
        app.ui.transcript_view.expanded.thinking.is_empty(),
        "the same key collapses what it expanded"
    );
}

/// With a cursor, the row under it decides. A reasoning summary the cursor
/// sits on expands even though a delegation follows it in the transcript,
/// which the tail-most rule would otherwise pick.
#[test]
fn test_ctrl_o_follows_cursor() {
    let mut app = working_app();
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::ThoughtFor {
            ms: Some(19_000),
            reasoning: Some("a long train of reasoning".into()),
            tool_summary: None,
            turn_id: "t1".into(),
        });
    app.sessions
        .current
        .transcript
        .push(TranscriptLine::Subagent {
            child_sid: "c1".into(),
            subagent_type: "explore".into(),
            summary: "found auth".into(),
            prompt: String::new(),
            folded_transcript: Vec::new(),
            color: None,
        });
    drop(render_text(&app, 80, 24));
    let ri = app
        .ui
        .frame
        .layout
        .last_transcript_rows
        .borrow()
        .iter()
        .position(|(_, text)| text.contains("Thought for"))
        .expect("the reasoning row is on screen");
    let top = app.whole_scroll().top_offset();
    app.ui.selection.transcript.start(0, top + ri);
    handle_ctrl_o(&mut app);
    assert!(
        app.ui
            .transcript_view
            .expanded
            .thinking
            .contains(&"t1".into()),
        "the row under the cursor expands"
    );
    assert!(
        app.ui.transcript_view.expanded.subagents.is_empty(),
        "the later delegation stays collapsed"
    );
}

mod agents_pane;
mod permission;

/// The pane's contract says an ordinary character key is not intercepted: it
/// closes the pane and lands in the input box, which is visible by then. The
/// shared pane policy would otherwise drop it into a box the pane covers.
#[test]
fn test_char_closes_trajectory_pane() {
    let mut app = working_app();
    app.ui.navigation.pane = Pane::Trajectory;
    handle_working(&mut app, key(KeyCode::Char('h')));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "a character key closes the trajectory pane"
    );
    assert_eq!(
        app.ui.navigation.viewport,
        ViewportMode::Working,
        "and leaves the viewport that drew the pane"
    );
    assert_eq!(
        app.ui.input.editor.value(),
        "h",
        "and the character lands in the visible input box"
    );
}

/// Focus draws the pane full screen, so the pane's keys must reach it there
/// too: a pane rendered but dead would take no character, no Enter, no
/// Home/End, and its Esc would fold the viewport instead of closing.
#[test]
fn test_focus_routes_trajectory_char() {
    let mut app = working_app();
    app.ui.navigation.viewport = ViewportMode::Focus;
    app.ui.navigation.pane = Pane::Trajectory;
    handle_working(&mut app, key(KeyCode::Char('h')));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "a character closes the pane in Focus too"
    );
    assert_eq!(
        app.ui.navigation.viewport,
        ViewportMode::Working,
        "and the viewport it was drawn in goes back to the working one, or the
         editor the character lands in would not be on screen"
    );
    assert_eq!(
        app.ui.input.editor.value(),
        "h",
        "and lands in the editor rather than being dropped"
    );
}

/// Focus must not steal the pane's Esc: the pane returns a level instead of
/// the viewport folding away from under it.
/// Esc at the pane's root closes it, and closing it must leave the viewport
/// that drew it, exactly as a character does.
#[test]
fn test_focus_trajectory_esc_closes() {
    let mut app = working_app();
    app.ui.navigation.viewport = ViewportMode::Focus;
    app.ui.navigation.pane = Pane::Trajectory;
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Transcript,
        "Esc at the root closes the pane"
    );
    assert_eq!(
        app.ui.navigation.viewport,
        ViewportMode::Working,
        "and the viewport goes back to the working one"
    );
}

#[test]
fn test_focus_trajectory_esc_levels() {
    let mut app = working_app();
    app.ui.navigation.viewport = ViewportMode::Focus;
    app.ui.navigation.pane = Pane::Trajectory;
    app.ui.trajectory.set_level(1);
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.ui.trajectory.level(),
        0,
        "Esc backs the pane a level instead of folding the viewport"
    );
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Trajectory,
        "the pane stays open one level down"
    );
    assert_eq!(
        app.ui.navigation.viewport,
        ViewportMode::Focus,
        "the viewport is untouched"
    );
}

/// Down at the last input line moves the keyboard into the child strip; with
/// no strip the key keeps its history meaning.
#[test]
fn test_down_enters_strip() {
    let mut app = working_app();
    app.sessions
        .current
        .capabilities
        .fleet
        .entries
        .push(FleetEntry {
            agent_id: "c1".into(),
            subagent_type: "verify".into(),
            turn: 1,
            tokens: 0,
            tool_uses: 0,
            last_activity: None,
            completed: None,
            completed_at: None,
            started_at: None,
        });
    app.ui.input.editor.set("a".into());
    handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert!(
        app.ui.fleet.is_focused(),
        "the strip below the input takes the keyboard"
    );
    assert_eq!(app.ui.input.editor.value(), "a", "the draft is untouched");
}
