//! Pins the frame order the transcript's turn rule reads: a message delivered
//! into a running turn is written as the message chunk immediately followed by
//! the mark that says so, and a message that opens a turn is written without
//! one. The mark is what tells the two apart, and a reader walking the stream
//! finds it beside the message.

use super::*;
use houyicoder_context::{EventId, SessionId};
use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::frontend::FrontendEvent;

fn entry(event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event,
    }
}

/// The method of the mark frame at index 1, or None when the event carries no
/// mark at all.
fn mark_method(frames: &[FrontendEvent]) -> Option<AcpxMethod> {
    match frames.get(1) {
        Some(FrontendEvent::Acpx { notification }) => Some(notification.method.clone()),
        _ => None,
    }
}

#[test]
fn test_mid_turn_input_marked() {
    let frames = Server::project_turn_event(&entry(SessionEvent::MidTurnInput {
        text: "steer here".into(),
        pending_input_id: None,
    }));
    assert!(
        matches!(frames.first(), Some(FrontendEvent::SessionUpdate { .. })),
        "the message itself comes first: {frames:?}"
    );
    assert_eq!(
        mark_method(&frames),
        Some(AcpxMethod::ContextMidTurnInput),
        "the delivery mark rides immediately after the message: {frames:?}"
    );
}

#[test]
fn test_child_notice_marked() {
    let frames = Server::project_turn_event(&entry(SessionEvent::NotificationInjected {
        child_session_id: "child-1".into(),
        turn: 1,
        order: 0,
        topic: "done".into(),
        summary: "found it".into(),
    }));
    assert_eq!(
        mark_method(&frames),
        Some(AcpxMethod::ContextChildCompleted),
        "a child completion is marked as delivered too: {frames:?}"
    );
}

#[test]
fn test_interrupt_notice_marked() {
    // The notice a regenerate leaves behind belongs to the turn it interrupts.
    // Read as a message that opens a turn, it ends the running turn and starts
    // another, so one turn's work renders as two summary rows.
    let frames = Server::project_turn_event(&entry(SessionEvent::TurnAborted {
        reason: "interrupted by user".into(),
    }));
    assert!(
        matches!(frames.first(), Some(FrontendEvent::SessionUpdate { .. })),
        "the visible notice still comes first: {frames:?}"
    );
    assert_eq!(
        mark_method(&frames),
        Some(AcpxMethod::ContextTurnInterrupted),
        "the interrupt notice carries its mark: {frames:?}"
    );
}

#[test]
fn test_prompt_unmarked() {
    let frames = Server::project_turn_event(&entry(SessionEvent::UserInput { text: "go".into() }));
    assert_eq!(frames.len(), 1, "a prompt carries no mark: {frames:?}");
    assert!(matches!(frames[0], FrontendEvent::SessionUpdate { .. }));
}
