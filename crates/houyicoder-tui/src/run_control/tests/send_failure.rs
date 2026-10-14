//! Send-failure contracts for the B1 paths: a dead driver must not leave
//! fake running state, dropped cards, or server-mirror ghosts behind. Uses
//! the connection_lost fixture so every send is deterministically refused;
//! each test asserts the user-visible refusal plus untouched state.

#![cfg(test)]

use crate::agent_message::{ServerEvent, SessionMessage};
use crate::app::handle_key;
use crate::ask_question_model::tests::card::single_input;
use crate::input::queue::InputItem;
use crate::pending_prompt::PendingPrompt;
use crate::records::AskQuestion;
use crate::records::{Approval, TranscriptLine};
use crate::state::Screen;
use crate::test_harness::connection_lost_app;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::trust::TrustPrompt;

fn last_line(app: &crate::state::App) -> String {
    app.sessions
        .current
        .transcript
        .iter()
        .rev()
        .find_map(|l| match l {
            TranscriptLine::System(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// A refused send must not fake a running turn: no busy spinner, no run
/// start, no reserved request, no optimistic echo.
#[test]
fn test_spawn_refused() {
    let mut app = connection_lost_app();
    app.spawn_run("hello".into());
    assert!(
        !app.agent_busy(),
        "a refused send must not fake a running turn"
    );
    assert!(app.run_started().is_none(), "no run start without delivery");
    assert!(
        app.active_run_req_id().is_none(),
        "no pending run id without delivery"
    );
    assert!(
        app.sessions.current.last_run_input.is_none(),
        "input stays editable"
    );
    assert!(
        !app.sessions
            .current
            .transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(_))),
        "no optimistic user echo without delivery"
    );
    assert!(
        last_line(&app).contains("connection lost"),
        "the refusal is visible: {}",
        last_line(&app)
    );
}

/// A refused child injection must not leave the optimistic echo or the
/// pending echo behind.
#[test]
fn test_child_inject_refused() {
    let mut app = connection_lost_app();
    app.ui.teammate_view = Some(crate::records::TeammateView {
        child_sid: "c1".into(),
        ..Default::default()
    });
    // A fleet entry for the viewed child (not completed) is what makes the
    // send take the injection branch.
    app.handle_agent_message(SessionMessage::Event(ServerEvent::AgentStatus {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 1,
        tokens: 0,
        tool_uses: 0,
        last_activity: None,
        completed: None,
    }));
    app.spawn_run(" steer text ".into());
    let view = app.ui.teammate_view.as_ref().expect("view stays open");
    assert!(
        view.transcript.is_empty(),
        "no optimistic echo without delivery: {:?}",
        view.transcript
    );
    assert!(
        view.pending_echo.is_none(),
        "no pending echo without delivery"
    );
    assert!(
        last_line(&app).contains("connection lost"),
        "the refusal is visible: {}",
        last_line(&app)
    );
}

/// A refused queue promotion keeps the head in place: no dropped input, no
/// fake run, no ghost server-mirror removal.
#[test]
fn test_drain_refused() {
    let mut app = connection_lost_app();
    app.ui.input.queue.push(InputItem::Message("head".into()));
    let promoted = app.drain_pending_head();
    assert!(!promoted, "a refused send must not report a promoted turn");
    assert_eq!(
        app.ui.input.queue.len(),
        1,
        "the head stays queued when the send is refused"
    );
    assert!(
        !app.agent_busy(),
        "a refused send must not fake a running turn"
    );
    assert!(
        last_line(&app).contains("connection lost"),
        "the refusal is visible: {}",
        last_line(&app)
    );
}

/// A refused re-drain of a parked head re-parks it as a parked message: the
/// user input is never dropped when the run cannot start, and the item stays
/// frontend-only (it had no server mirror to begin with).
#[test]
fn test_drain_reparks_parked() {
    let mut app = connection_lost_app();
    app.ui
        .input
        .queue
        .push(InputItem::ParkedMessage("head".into()));
    let promoted = app.drain_pending_head();
    assert!(!promoted, "a refused send must not report a promoted turn");
    assert_eq!(app.ui.input.queue.len(), 1, "the parked head stays queued");
    assert!(
        matches!(
            app.ui.input.queue.first(),
            Some(InputItem::ParkedMessage(_))
        ),
        "the head re-parks as a parked message, not a live mirror"
    );
    assert!(
        !app.agent_busy(),
        "a refused send must not fake a running turn"
    );
    assert!(
        last_line(&app).contains("connection lost"),
        "the refusal is visible: {}",
        last_line(&app)
    );
}

/// A refused verdict keeps the card and the request id: the server is still
/// waiting, so the approval must not disappear.
#[test]
fn test_verdict_refused() {
    let mut app = connection_lost_app();
    app.sessions.current.prompt = Some(PendingPrompt::approval_card(
        RequestId(7),
        Approval {
            tool: "bash".into(),
            args: r#"{"command":"ls"}"#.into(),
            reason: "wants to run".into(),
            selected: 0,
            call_id: "c1".into(),
            options: Vec::new(),
            ..Default::default()
        },
    ));
    app.resolve_current_approval(crate::run_control::ApprovalDecision {
        call_id: "c1".into(),
        approved: true,
        updated_input: None,
        scope: "once".into(),
    });
    assert!(
        app.approval().is_some(),
        "the card stays until the verdict is delivered"
    );
    assert!(
        app.sessions.current.prompt.as_ref().map(|p| p.req_id()) == Some(RequestId(7)),
        "the request id stays until the verdict is delivered"
    );
    assert!(
        !app.agent_busy(),
        "the run does not resume without a delivered verdict"
    );
    assert!(
        last_line(&app).contains("connection lost"),
        "the refusal is visible: {}",
        last_line(&app)
    );
}

/// A refused question verdict keeps the question card: the server is still
/// waiting, so the answer must not vanish when its verdict cannot ship.
#[test]
fn test_question_verdict_refused() {
    let mut app = connection_lost_app();
    app.ui.navigation.screen = Screen::Working;
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse question");
    app.sessions.current.prompt = Some(PendingPrompt::question_card(RequestId(7), aq));
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        app.ask_question().is_some(),
        "the question card stays until the verdict is delivered"
    );
    assert!(
        app.sessions.current.prompt.as_ref().map(|p| p.req_id()) == Some(RequestId(7)),
        "the request id stays until the verdict is delivered"
    );
    assert!(
        last_line(&app).contains("connection lost"),
        "the refusal is visible: {}",
        last_line(&app)
    );
}

/// A refused question cancel keeps the question card: declining is still a
/// verdict, so it must not vanish the card when that verdict cannot ship.
#[test]
fn test_question_cancel_refused() {
    let mut app = connection_lost_app();
    app.ui.navigation.screen = Screen::Working;
    let aq = AskQuestion::parse("c1", &single_input()).expect("parse question");
    app.sessions.current.prompt = Some(PendingPrompt::question_card(RequestId(7), aq));
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        app.ask_question().is_some(),
        "the question card stays until the decline verdict is delivered"
    );
    assert!(
        last_line(&app).contains("connection lost"),
        "the refusal is visible: {}",
        last_line(&app)
    );
}

/// A refused trust verdict keeps the prompt; the local exit only happens
/// after the rejection was actually delivered.
#[test]
fn test_trust_refused() {
    let mut app = connection_lost_app();
    app.sessions.current.prompt = Some(PendingPrompt::trust_ask(
        RequestId(3),
        TrustPrompt {
            project_path: "/proj".into(),
            risks: Vec::new(),
        },
    ));
    app.resolve_trust(false);
    assert!(
        app.pending_trust().is_some(),
        "the trust prompt stays until the verdict is delivered"
    );
    assert!(!app.ui.quit, "a refused rejection must not exit the TUI");
    assert!(
        last_line(&app).contains("connection lost"),
        "the refusal is visible: {}",
        last_line(&app)
    );
}

/// A refused abort must not enter the cancelling state: cancellation that
/// never reaches the driver cannot wait for a completion that clears it.
#[test]
fn test_abort_refused() {
    let mut app = connection_lost_app();
    app.start_run_for_test(0);
    app.abort_run();
    assert!(
        !app.cancelling(),
        "a refused abort must not fake the cancelling state"
    );
    assert!(
        last_line(&app).contains("connection lost"),
        "the refusal is visible: {}",
        last_line(&app)
    );
}

/// A refused mirror removal keeps the queued message: recalling locally
/// while the server copy stays would duplicate the input on drain.
#[test]
fn test_recall_refused() {
    let mut app = connection_lost_app();
    app.ui.input.queue.push(InputItem::Message("head".into()));
    app.pop_queued_to_input();
    assert_eq!(
        app.ui.input.queue.len(),
        1,
        "the queued message stays while its server copy exists"
    );
    assert!(
        app.ui.input.editor.value().is_empty(),
        "nothing is recalled into the editor when the mirror removal fails"
    );
    assert!(
        last_line(&app).contains("connection lost"),
        "the refusal is visible: {}",
        last_line(&app)
    );
}
