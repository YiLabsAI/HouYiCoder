use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::model::{
    AppliedModel, EffectiveFrom, ModelApplyResult, ModelCatalog, ModelChoice, PersistenceOutcome,
    SpeedMode,
};
use houyicoder_protocol::llm::EffortLevel;
use std::time::{Duration, Instant};

use crate::records::TranscriptLine;
use crate::session::capabilities::model::PendingCommit;
use crate::state::{Pane, Screen};
use crate::test_harness::{
    TransportEvent, connected_app_with_events, model_app, model_caps, model_catalog, model_entry,
};

use crate::agent_message::{ServerEvent, ServerResponse, SessionMessage};
use crate::composition::app;
use crate::keys::handle_working;
use crate::test_harness::connection_lost_app;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn model_response(req_id: u64, result: ModelApplyResult) -> SessionMessage {
    SessionMessage::Response {
        request: RequestId(req_id),
        response: ServerResponse::Model { result },
    }
}

fn model_info_response(req_id: u64, catalog: ModelCatalog) -> SessionMessage {
    SessionMessage::Response {
        request: RequestId(req_id),
        response: ServerResponse::ModelInfo { catalog },
    }
}

/// A ModelResult moves the session's applied state and renders the receipt
/// from the reply: the status line names what the host applied, never the
/// draft the pane was holding.
#[test]
fn test_model_result_updates_status() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Model;
    app.sessions.current.capabilities.model.pending = Some(PendingCommit {
        req_id: RequestId(1),
        prior_speed: SpeedMode::Standard,
    });
    app.handle_agent_message(model_response(
        1,
        apply_result("qwen3.8-max", Some(EffortLevel::High), SpeedMode::Fast),
    ));
    assert_eq!(
        app.sessions.current.capabilities.model.catalog.applied.id, "qwen3.8-max",
        "the applied model comes from the reply"
    );
    assert_eq!(
        app.sessions.current.capabilities.model.catalog.applied.id, "qwen3.8-max",
        "status.model follows the applied model"
    );
    assert_eq!(app.ui.navigation.pane, Pane::Transcript, "the pane closes");
    let last = app
        .sessions
        .current
        .transcript
        .last()
        .expect("a receipt line");
    assert!(
        matches!(last, TranscriptLine::System(s) if s.contains("qwen3.8-max")),
        "the receipt carries the actual id: {last:?}"
    );
}

/// The receipt is formatted from the apply result: the label the user picked,
/// the id the provider sees, the effort that will be sent and the Fast tier.
#[test]
fn test_receipt_from_apply_result() {
    let mut app = app();
    app.refresh_model_catalog(model_catalog(vec![model_entry(
        "qwen3.8-max",
        "Max",
        model_caps(true, true),
    )]));
    app.handle_agent_message(model_response(
        1,
        apply_result("qwen3.8-max", Some(EffortLevel::High), SpeedMode::Fast),
    ));
    let last = app
        .sessions
        .current
        .transcript
        .last()
        .expect("a receipt line");
    let TranscriptLine::System(text) = last else {
        panic!("expected a system line, got {last:?}");
    };
    assert_eq!(
        text, "Model set to Max (qwen3.8-max) · high effort · Fast mode on",
        "the receipt names the pick and the settings that apply"
    );
}

/// The Default pick reads as the sentinel with the resolved id: the receipt
/// explains which model the session now sends instead of printing the bare
/// choice.
#[test]
fn test_receipt_names_default_target() {
    let mut app = app();
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(1),
        response: ServerResponse::Model {
            result: default_result("qwen3.7-max"),
        },
    });
    let last = app
        .sessions
        .current
        .transcript
        .last()
        .expect("a receipt line");
    assert!(
        matches!(last, TranscriptLine::System(s) if s.starts_with("Model set to Default (qwen3.7-max)")),
        "Default receipt names the resolved model: {last:?}"
    );
    assert!(
        !app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s)
            if s.trim() == "model: Default")),
        "no bare model: Default line: {:?}",
        app.sessions.current.transcript
    );
}

/// A ModelResult that did not reach disk says so: the session switched, but a
/// restart would lose the pick.
#[test]
fn test_receipt_reports_session_only() {
    let mut app = app();
    let mut result = apply_result("qwen3.8-max", None, SpeedMode::Standard);
    result.persistence = PersistenceOutcome::Partial {
        settings: Some("read-only settings".into()),
        session_record: None,
    };
    app.handle_agent_message(model_response(1, result));
    let last = app
        .sessions
        .current
        .transcript
        .last()
        .expect("a receipt line");
    assert!(
        matches!(last, TranscriptLine::System(s)
            if s.contains("settings not saved: read-only settings")),
        "the failed destination is named verbatim: {last:?}"
    );
}

/// A pick that lands while a run is in flight says it applies to the next
/// model request rather than implying the running one changed.
#[test]
fn test_receipt_reports_boundary() {
    let mut app = app();
    let mut result = apply_result("glm-5.2", None, SpeedMode::Standard);
    result.effective_from = EffectiveFrom::NextRequest;
    app.handle_agent_message(model_response(1, result));
    let last = app
        .sessions
        .current
        .transcript
        .last()
        .expect("a receipt line");
    assert!(
        matches!(last, TranscriptLine::System(s)
            if s.ends_with("· applies to the next model request")),
        "the boundary is named: {last:?}"
    );
}

/// A SystemLine notice renders verbatim as a transcript system line (an
/// overflow the catalog could not self-heal, pointing at the override).
#[test]
fn test_system_line_renders_notice() {
    let mut app = app();
    app.handle_agent_message(SessionMessage::Event(ServerEvent::SystemLine {
        text: "set catalog context_window".into(),
    }));
    assert!(
        app.sessions.current.transcript.iter().any(
            |l| matches!(l, TranscriptLine::System(s) if s.contains("set catalog context_window"))
        ),
        "system line lands in the transcript"
    );
}

/// A ModelInfoResult replaces the catalog, and a clean draft re-seeds onto
/// the selection the host reports.
#[test]
fn test_model_info_stashes_catalog() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Model;
    let mut catalog = model_catalog(vec![
        model_entry("a", "A", model_caps(true, false)),
        model_entry("b", "B", model_caps(true, false)),
    ]);
    catalog.selected = ModelChoice::Explicit { id: "b".into() };
    app.handle_agent_message(model_info_response(4, catalog));
    assert_eq!(
        app.sessions
            .current
            .capabilities
            .model
            .catalog
            .entries
            .len(),
        2,
        "the rows are stashed"
    );
    assert_eq!(
        app.ui.model.row, 2,
        "the draft sits on the selection the host reports"
    );
}

/// A ModelInfoResult that lands after the user started editing leaves the
/// draft alone: the refresh must not undo a pick in progress.
#[test]
fn test_model_info_keeps_draft() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Model;
    app.refresh_model_catalog(model_catalog(vec![
        model_entry("a", "A", model_caps(true, false)),
        model_entry("b", "B", model_caps(true, false)),
    ]));
    app.move_model_focus(1);
    assert_eq!(
        app.ui.model.row, 2,
        "the draft moved onto the second catalog row"
    );
    app.handle_agent_message(model_info_response(
        4,
        model_catalog(vec![model_entry("a", "A", model_caps(true, false))]),
    ));
    assert_eq!(
        app.ui.model.row, 2,
        "a dirty draft survives the catalog landing"
    );
}

/// A reply whose selection matches no catalog row leaves the draft on the
/// Default row rather than pointing at a row that does not exist.
#[test]
fn test_missing_selection_falls_back() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Model;
    let mut catalog = model_catalog(vec![model_entry("a", "A", model_caps(true, false))]);
    catalog.selected = ModelChoice::Explicit {
        id: "not-here".into(),
    };
    app.handle_agent_message(model_info_response(4, catalog));
    assert_eq!(
        app.ui.model.row, 0,
        "an unroutable selection falls back to the Default row"
    );
}

/// When a Model pane is open, typing printable chars does not push them into
/// the input box — the pane owns the keyboard.
#[test]
fn test_model_pane_swallows_chars() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Model;
    handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    );
    assert!(
        app.ui.input.editor.value().is_empty(),
        "char swallowed by Model pane, not pushed to input: {}",
        app.ui.input.editor.value()
    );
}

/// Adjusting the draft moves nothing but the draft: the applied model, the
/// status line and the store of record hold the session's real state until
/// the host answers.
#[test]
fn test_picker_draft_isolated() {
    let mut catalog = model_catalog(vec![
        model_entry("a", "A", model_caps(true, true)),
        model_entry("b", "B", model_caps(true, true)),
    ]);
    catalog.applied.id = "a".into();
    let mut app = model_app(catalog);
    app.ui.navigation.pane = Pane::Model;
    let start = app.ui.model.row;
    let status_before = app
        .sessions
        .current
        .capabilities
        .model
        .catalog
        .applied
        .id
        .clone();
    app.move_model_focus(1);
    app.adjust_model_setting(true);
    app.cycle_model_setting();
    app.adjust_model_setting(true);
    assert_eq!(
        app.sessions.current.capabilities.model.catalog.applied.id, "a",
        "editing the draft does not move the applied model"
    );
    assert_ne!(app.ui.model.row, start, "the draft did move");
    assert_eq!(
        app.sessions.current.capabilities.model.catalog.applied.id, status_before,
        "the status line is untouched until the host answers"
    );
}

/// A commit ships exactly one request: a second Enter while the first is in
/// flight cannot ship a duplicate switch.
#[test]
fn test_model_pending_blocks_repeat() {
    let (mut app, events) = connected_app_with_events();
    app.open_model_pane();
    // The commit needs the rows the host's query reply carries; seed them as
    // the reply would.
    app.sessions.current.capabilities.model.catalog =
        model_catalog(vec![model_entry("a", "A", model_caps(true, false))]);
    app.reseed_model_draft();
    app.commit_model_pick();
    let req_id = app
        .sessions
        .current
        .capabilities
        .model
        .pending
        .as_ref()
        .expect("the commit is in flight")
        .req_id;
    app.commit_model_pick();
    app.commit_model_pick();
    // Latch on the shipped frame, then watch for a quiet window: a duplicate
    // would arrive within it, and no duplicate means one switch shipped.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut swaps = 0;
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(TransportEvent::Frame(frame)) if frame.contains("ModelSet") => {
                swaps += 1;
                if swaps > 1 {
                    break;
                }
            }
            Ok(_) => {}
            Err(_) if swaps > 0 => break,
            Err(_) => {}
        }
    }
    assert_eq!(swaps, 1, "repeated Enter ships one ModelSet");

    // A refused commit settles without a switch: the draft survives and a
    // retry ships again.
    app.fail_model_pick(req_id, "no such model");
    assert!(
        !app.sessions.current.capabilities.model.is_pending(),
        "the failure settles it"
    );
    assert!(
        app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s)
            if s == "model: no such model")),
        "the reason reaches the transcript: {:?}",
        app.sessions.current.transcript
    );
}

/// A dead driver refuses the switch: no applied change, the pane stays open
/// for retry, and the refusal names the lost connection.
#[test]
fn test_model_send_failure() {
    let mut app = connection_lost_app();
    app.refresh_model_catalog(model_catalog(vec![model_entry(
        "glm-5.2",
        "Fable",
        model_caps(true, true),
    )]));
    app.ui.navigation.pane = Pane::Model;
    let applied_before = app
        .sessions
        .current
        .capabilities
        .model
        .catalog
        .applied
        .clone();
    app.commit_model_pick();
    assert_eq!(
        app.sessions.current.capabilities.model.catalog.applied, applied_before,
        "the applied model stays untouched"
    );
    assert!(
        !app.sessions.current.capabilities.model.is_pending(),
        "a switch that never shipped is not pending"
    );
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Model,
        "the pane stays open for retry"
    );
    let last = app
        .sessions
        .current
        .transcript
        .last()
        .expect("a line was pushed");
    match last {
        TranscriptLine::System(text) => {
            assert!(
                text.contains("model: connection lost"),
                "expected connection-lost, got {text}"
            );
        }
        other => panic!("expected a system line, got {other:?}"),
    }
}

/// M-34: a draft holding Fast that moves onto a model with no tier commits it
/// off. The request carries Standard, and the receipt writes the change the
/// downgrade made rather than passing the tier over in silence.
#[test]
fn test_fast_degrades_on_commit() {
    let (mut app, events) = connected_app_with_events();
    app.refresh_model_catalog(model_catalog(vec![
        model_entry("qwen3.8-max", "Max", model_caps(true, true)),
        model_entry("plain-model", "Plain", model_caps(true, false)),
    ]));
    app.ui.navigation.pane = Pane::Model;
    // The session ran Fast; the focus moves to the row that cannot serve it.
    app.sessions
        .current
        .capabilities
        .model
        .catalog
        .applied
        .speed = SpeedMode::Fast;
    app.ui.model.speed = SpeedMode::Fast;
    app.ui.model.row = 2;
    assert_eq!(
        app.effective_model_speed(),
        SpeedMode::Standard,
        "the draft does not ask for a tier this row cannot serve"
    );
    app.commit_model_pick();
    let frame = loop {
        match events.recv_timeout(Duration::from_secs(5)) {
            Ok(TransportEvent::Frame(frame)) if frame.contains("ModelSet") => break frame,
            Ok(_) => {}
            Err(why) => panic!("no ModelSet shipped: {why}"),
        }
    };
    assert!(
        frame.contains("plain-model") && frame.contains("standard") && !frame.contains("fast"),
        "the shipped request carries the tier the row can serve: {frame}"
    );
    let pending_id = app
        .sessions
        .current
        .capabilities
        .model
        .pending
        .as_ref()
        .expect("the commit is in flight")
        .req_id;
    app.handle_agent_message(model_response(
        pending_id.0,
        apply_result("plain-model", None, SpeedMode::Standard),
    ));
    let last = app
        .sessions
        .current
        .transcript
        .last()
        .expect("a receipt line");
    assert!(
        matches!(last, TranscriptLine::System(s)
            if s == "Model set to Plain (plain-model) · Fast mode off"),
        "the receipt reports the downgrade: {last:?}"
    );
}

/// The applied-effort store follows the reply, so the badge reports what the
/// next request carries (None drops the badge).
#[test]
fn test_model_result_stashes_effort() {
    let mut app = app();
    app.handle_agent_message(model_response(
        1,
        apply_result("qwen3.7-max", Some(EffortLevel::High), SpeedMode::Standard),
    ));
    assert_eq!(
        app.sessions
            .current
            .capabilities
            .model
            .catalog
            .applied
            .effort,
        Some(EffortLevel::High),
        "effort stashed"
    );
    app.handle_agent_message(model_response(
        1,
        apply_result("deepseek-chat", None, SpeedMode::Standard),
    ));
    assert!(
        app.sessions
            .current
            .capabilities
            .model
            .catalog
            .applied
            .effort
            .is_none(),
        "None clears the badge"
    );
}

/// A reply the pane is not awaiting still moves the session: the host applied
/// it, and the transcript must report that rather than swallow it. But it
/// must not settle the commit the pane is holding — that one's own reply
/// decides it.
#[test]
fn test_unmatched_result_still_applies() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Model;
    app.sessions.current.capabilities.model.pending = Some(PendingCommit {
        req_id: RequestId(2),
        prior_speed: SpeedMode::Standard,
    });
    app.handle_agent_message(model_response(
        1,
        apply_result("qwen3.8-max", None, SpeedMode::Standard),
    ));
    assert_eq!(
        app.sessions.current.capabilities.model.catalog.applied.id,
        "qwen3.8-max"
    );
    assert!(
        app.sessions.current.capabilities.model.is_pending(),
        "a stale reply leaves the held commit in flight"
    );
    assert_eq!(
        app.ui.navigation.pane,
        Pane::Model,
        "a stale reply does not close the pane"
    );
    app.handle_agent_message(model_response(
        2,
        apply_result("glm-5.2", None, SpeedMode::Standard),
    ));
    assert!(
        !app.sessions.current.capabilities.model.is_pending(),
        "the held commit's own reply settles it"
    );
    assert_eq!(app.ui.navigation.pane, Pane::Transcript);
}

/// A stale reply moves the session but must not wipe the draft the user is
/// editing: only the held commit's own reply reseeds.
#[test]
fn test_stale_reply_keeps_draft() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Model;
    app.sessions.current.capabilities.model.pending = Some(PendingCommit {
        req_id: RequestId(2),
        prior_speed: SpeedMode::Standard,
    });
    app.ui.model.row = 1;
    app.ui.model.dirty = true;
    app.handle_agent_message(model_response(
        1,
        apply_result("qwen3.8-max", None, SpeedMode::Standard),
    ));
    assert!(app.ui.model.dirty, "the draft survives the stale reply");
    assert_eq!(
        app.ui.model.row, 1,
        "the user's in-progress focus row survives"
    );
    app.handle_agent_message(model_response(
        2,
        apply_result("glm-5.2", None, SpeedMode::Standard),
    ));
    assert!(!app.ui.model.dirty, "the settling reply reseeds the draft");
}

fn apply_result(id: &str, effort: Option<EffortLevel>, speed: SpeedMode) -> ModelApplyResult {
    ModelApplyResult {
        selected: ModelChoice::Explicit { id: id.to_string() },
        applied: AppliedModel {
            id: id.to_string(),
            effort,
            speed,
        },
        effective_from: EffectiveFrom::Immediate,
        persistence: PersistenceOutcome::Saved,
    }
}

fn default_result(id: &str) -> ModelApplyResult {
    ModelApplyResult {
        selected: ModelChoice::Default,
        applied: AppliedModel {
            id: id.to_string(),
            effort: None,
            speed: SpeedMode::Standard,
        },
        effective_from: EffectiveFrom::Immediate,
        persistence: PersistenceOutcome::Saved,
    }
}
