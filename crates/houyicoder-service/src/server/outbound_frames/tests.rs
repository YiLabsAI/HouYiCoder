//! Pins the frame order the transcript's turn rule reads: a message delivered
//! into a running turn is written as the message chunk immediately followed by
//! the mark that says so, and a message that opens a turn is written without
//! one. The mark is what tells the two apart, and a reader walking the stream
//! finds it beside the message.
//!
//! It also pins what the same path writes for an event too large to send:
//! a bounded notice in the event's stream position, so the connection survives
//! and the consumer finds a row rather than a gap.

use super::*;
use futures::StreamExt;
use futures::channel::mpsc;
use houyicoder_context::{EventId, SessionId};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::acpx::AcpxMethod;
use houyicoder_protocol::envelope::EventSeq;
use houyicoder_protocol::framing::{FrameDecoder, MAX_MESSAGE_BYTES};
use houyicoder_protocol::frontend::FrontendEvent;
use houyicoder_session::SessionStore;

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
    let mapped = Server::map_turn_event(&entry(SessionEvent::MidTurnInput {
        text: "steer here".into(),
        pending_input_id: None,
    }));
    let frames = &mapped.payloads;
    assert!(
        matches!(frames.first(), Some(FrontendEvent::SessionUpdate { .. })),
        "the message itself comes first: {frames:?}"
    );
    assert_eq!(
        mark_method(frames),
        Some(AcpxMethod::ContextMidTurnInput),
        "the delivery mark rides immediately after the message: {frames:?}"
    );
}

#[test]
fn test_child_notice_marked() {
    let mapped = Server::map_turn_event(&entry(SessionEvent::NotificationInjected {
        child_session_id: "child-1".into(),
        turn: 1,
        order: 0,
        topic: "done".into(),
        summary: "found it".into(),
    }));
    assert_eq!(
        mark_method(&mapped.payloads),
        Some(AcpxMethod::ContextChildCompleted),
        "a child completion is marked as delivered too: {:?}",
        mapped.payloads
    );
}

#[test]
fn test_interrupt_notice_marked() {
    // The notice a regenerate leaves behind belongs to the turn it interrupts.
    // Read as a message that opens a turn, it ends the running turn and starts
    // another, so one turn's work renders as two summary rows.
    let mapped = Server::map_turn_event(&entry(SessionEvent::TurnAborted {
        reason: "interrupted by user".into(),
    }));
    let frames = &mapped.payloads;
    assert!(
        matches!(frames.first(), Some(FrontendEvent::SessionUpdate { .. })),
        "the visible notice still comes first: {frames:?}"
    );
    assert_eq!(
        mark_method(frames),
        Some(AcpxMethod::ContextTurnInterrupted),
        "the interrupt notice carries its mark: {frames:?}"
    );
}

#[test]
fn test_prompt_unmarked() {
    let mapped = Server::map_turn_event(&entry(SessionEvent::UserInput { text: "go".into() }));
    let frames = &mapped.payloads;
    assert_eq!(frames.len(), 1, "a prompt carries no mark: {frames:?}");
    assert!(matches!(frames[0], FrontendEvent::SessionUpdate { .. }));
}

#[test]
fn test_entry_payloads_carry_identity() {
    // The identity the consumer stamps rows from is the log entry's own id
    // in transport form, so a reconnect that reshuffles sequence numbers
    // still lands on the same durable rows.
    let entry = entry(SessionEvent::UserInput { text: "go".into() });
    let mapped = Server::map_turn_event(&entry);
    assert_eq!(mapped.event_id.0, entry.id.to_string());
}

/// A carrier whose outbound frames land in the returned receiver. The inbound
/// side is closed, so nothing this unit reads can arrive.
fn carrier() -> (FrameCarrier, mpsc::Receiver<String>) {
    let (tx, outbound) = mpsc::channel::<String>(4);
    let (inbound_tx, inbound) = mpsc::channel::<String>(1);
    drop(inbound_tx);
    (FrameCarrier::new(tx, inbound), outbound)
}

/// The frame the carrier moved, decoded by the protocol's own decoder under the
/// protocol's own byte bound: whatever arrives is a frame the peer would admit.
async fn read_frame(outbound: &mut mpsc::Receiver<String>) -> ServerFrame {
    let line = outbound.next().await.expect("a frame reached the carrier");
    let mut decoder = FrameDecoder::new(MAX_MESSAGE_BYTES);
    decoder
        .push(line.as_bytes())
        .expect("the frame fits the protocol's per-message bound");
    decoder
        .next_frame::<ServerFrame>()
        .expect("a complete frame")
        .expect("decodes")
}

#[tokio::test]
async fn test_event_under_bound_passes() {
    let (mut io, mut outbound) = carrier();
    let frame = EventEnvelope::new(
        EventSeq(3),
        FrontendEvent::Message {
            delta: "hello".into(),
        },
    );
    Server::send_event_frame(&mut io, frame)
        .await
        .expect("sent");
    let sent = match read_frame(&mut outbound).await {
        ServerFrame::Event(env) => env,
        other => panic!("an event frame: {other:?}"),
    };
    assert_eq!(
        sent.seq,
        EventSeq(3),
        "an admitted event keeps its position"
    );
    assert!(
        matches!(&sent.payload, FrontendEvent::Message { delta } if delta == "hello"),
        "the admitted payload passes through unchanged: {:?}",
        sent.payload
    );
}

/// A history longer than one round is walked in rounds, each mapping its record
/// cap and reporting a position past the records it mapped, so the rounds join
/// up: every record of the log is mapped once, in the log's order, and the walk
/// ends on the history's own end.
///
/// The cap and the reported position are the pair a resume rests on. A round
/// that reported the position it was given would map the same records twice,
/// and one that reported a position short of what it mapped would skip the
/// records in between; both leave the history the client rebuilds wrong while
/// every round in isolation looks well formed.
#[tokio::test]
async fn test_history_walk_batches_capped() {
    let store = SessionStore::new(Box::new(InMemoryBackend::new()));
    let session = SessionId::new();
    let records = DURABLE_BATCH_RECORDS * 2;
    let mut written: Vec<String> = Vec::new();
    for turn in 0..records {
        let record = SessionLogEntry {
            id: EventId::new(),
            session,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput {
                text: format!("turn {turn}"),
            },
        };
        written.push(record.id.to_string());
        store.append(record).await.expect("the record is written");
    }
    let log: &dyn SessionLog = &store;
    let mut from = TrajectoryPosition::start();
    let mut read: Vec<String> = Vec::new();
    let mut rounds = 0;
    let mut finished = false;
    while rounds < 16 {
        rounds += 1;
        let batch = Server::load_entries(log, session, from);
        if !batch.walk_finished {
            assert!(
                batch.entries.len() >= DURABLE_BATCH_RECORDS,
                "a round with more to read maps its record cap: {} entries",
                batch.entries.len()
            );
        }
        // A page arrives whole, so a round can overshoot its cap by the records
        // of one page. What the cap bounds is a round's work, and this log is
        // several rounds long, so no one round maps all of it.
        assert!(
            batch.entries.len() < records,
            "no round maps the whole log: {} of {records} records",
            batch.entries.len()
        );
        assert!(
            batch.next.index >= from.index + batch.entries.len(),
            "a round resumes past the records it mapped: {} from {} plus {} entries",
            batch.next.index,
            from.index,
            batch.entries.len()
        );
        read.extend(batch.entries.iter().map(|e| e.event_id.0.clone()));
        if batch.walk_finished {
            finished = true;
            break;
        }
        from = batch.next;
    }
    assert!(finished, "the walk reaches the history's end");
    assert!(
        rounds >= 2,
        "a history longer than the cap takes more than one round: {rounds}"
    );
    assert_eq!(
        read, written,
        "the rounds map every record once, in the log's order"
    );
}

#[tokio::test]
async fn test_oversized_event_replaced() {
    let (mut io, mut outbound) = carrier();
    let frame = EventEnvelope::new(
        EventSeq(9),
        FrontendEvent::Message {
            delta: "x".repeat(MAX_MESSAGE_BYTES),
        },
    );
    Server::send_event_frame(&mut io, frame)
        .await
        .expect("the notice goes out in place of the event");
    let sent = match read_frame(&mut outbound).await {
        ServerFrame::Event(env) => env,
        other => panic!("an event frame: {other:?}"),
    };
    assert_eq!(
        sent.seq,
        EventSeq(9),
        "the notice stands where the event would have"
    );
    assert!(
        matches!(sent.payload, FrontendEvent::SystemLine { ref text } if text == OVERSIZED_EVENT_NOTICE),
        "the payload is the bounded notice: {:?}",
        sent.payload
    );
    assert!(
        sent.event_id.is_none(),
        "a replaced event must not claim the durable entry's identity"
    );
}
