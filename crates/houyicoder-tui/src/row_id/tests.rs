use super::*;
use crate::transcript::FrontendRow;
use houyicoder_protocol::acpx::{AcpxMethod, AcpxNotification};
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};
use serde_json::json;

fn session_frame() -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Text {
            text: "answer".into(),
        },
    )))
}

fn acpx_frame() -> TranscriptFrame {
    TranscriptFrame::Acpx(AcpxNotification::new(
        AcpxMethod::ContextRunCompleted,
        json!({"ms": 12}),
    ))
}

fn event_id() -> DurableEventId {
    DurableEventId("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string())
}

#[test]
fn test_slot_follows_category() {
    // The notification is the entry's only frame; its slot is still the
    // notification's position in the frame order, not zero.
    let lone = RowId::of(&acpx_frame(), Some(&event_id()), 9);
    assert_eq!(
        lone,
        RowId::Durable {
            event: event_id(),
            slot: 1
        }
    );
    let session = RowId::of(&session_frame(), Some(&event_id()), 9);
    assert_eq!(
        session,
        RowId::Durable {
            event: event_id(),
            slot: 0
        }
    );
}

#[test]
fn test_unstamped_frame_local() {
    let local = RowId::of(&session_frame(), None, 42);
    assert_eq!(local, RowId::local(42));
    assert!(!local.is_durable());
}

#[test]
fn test_frontend_frame_local() {
    let frame = TranscriptFrame::Frontend(FrontendRow::System("notice".into()));
    assert_eq!(RowId::of(&frame, Some(&event_id()), 7), RowId::local(7));
}

#[test]
fn test_display_formats() {
    let durable = RowId::Durable {
        event: event_id(),
        slot: 1,
    };
    assert_eq!(durable.to_string(), "e01ARZ3NDEKTSV4RRFFQ69G5FAV.1");
    assert_eq!(RowId::local(7).to_string(), "f7");
}

#[test]
fn test_names_matches_own_frame() {
    let id = RowId::of(&acpx_frame(), Some(&event_id()), 3);
    assert!(id.names(&acpx_frame(), Some(&event_id())));
    assert!(
        !id.names(&session_frame(), Some(&event_id())),
        "the same entry's other slot is a different row"
    );
    assert!(!id.names(&acpx_frame(), None));
    assert!(
        !RowId::local(3).names(&acpx_frame(), Some(&event_id())),
        "a local identity names no frame"
    );
}

#[test]
fn test_from_event_name() {
    assert_eq!(
        RowId::from("t1"),
        RowId::Durable {
            event: DurableEventId("t1".to_string()),
            slot: 0
        }
    );
}
