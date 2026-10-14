//! Tests for this module.

use super::*;
use agent_event::{
    MemoryChangeCausality as AgentCausality, MemoryChangeOrigin as AgentOrigin,
    MemoryOperation as AgentOperation,
};
use houyicoder_context::MemoryChangeId;
use std::sync::mpsc::channel;
use std::thread::spawn;

fn line(text: &str) -> FrontendEvent {
    FrontendEvent::SystemLine {
        text: text.to_string(),
    }
}

fn durable(id: &str, payloads: Vec<FrontendEvent>) -> EntryPayloads {
    EntryPayloads {
        event_id: DurableEventId(id.to_string()),
        payloads,
    }
}

fn status(agent_id: &str, turn: u32) -> FrontendEvent {
    FrontendEvent::AgentStatus {
        agent_id: agent_id.to_string(),
        subagent_type: "explore".to_string(),
        turn,
        tokens: 0,
        tool_uses: 0,
        last_activity: None,
        completed: None,
    }
}

#[test]
fn test_durable_precedes_preview() {
    let sequencer = EventSequencer::new();
    sequencer.enqueue_ephemeral(line("delta"));

    let frames =
        sequencer.sequence_available(|_| vec![durable("e1", vec![line("user"), line("commit")])]);

    assert_eq!(frames[0].seq, EventSeq(0));
    assert_eq!(frames[1].seq, EventSeq(1));
    assert_eq!(frames[2].seq, EventSeq(2));
}

#[test]
fn test_identity_rides_envelope() {
    // Every payload one entry maps to carries that entry's
    // identity, while a runtime push with no log entry carries none,
    // so a consumer can tell durable rows from local ones.
    let sequencer = EventSequencer::new();
    let frames =
        sequencer.sequence_available(|_| vec![durable("e1", vec![line("user"), line("commit")])]);
    assert_eq!(
        frames[0].event_id.as_ref().map(|id| &id.0),
        Some(&"e1".to_string())
    );
    assert_eq!(frames[1].event_id, frames[0].event_id);

    sequencer.enqueue_reliable(line("notice"));
    let pending = sequencer.sequence_pending_for_test();
    assert_eq!(pending[0].event_id, None);
}

#[test]
fn test_replay_preserves_identity() {
    // A reconnect replays retained envelopes; the durable identity must
    // survive retention so the resumed consumer lands on the same rows.
    let sequencer = EventSequencer::new();
    sequencer.sequence_available(|_| vec![durable("e1", vec![line("user")])]);
    sequencer.enqueue_reliable(line("notice"));
    drop(sequencer.sequence_pending_for_test());

    let replay = sequencer.reliable_after(None);
    assert_eq!(replay.len(), 2);
    assert_eq!(
        replay[0].event_id.as_ref().map(|id| &id.0),
        Some(&"e1".to_string())
    );
    assert_eq!(replay[1].event_id, None);
}

#[test]
fn test_replay_resumes_after_seq() {
    let sequencer = EventSequencer::new();
    let first = sequencer.sequence_reliable(line("first"));
    let second = sequencer.sequence_reliable(line("second"));
    let replay = sequencer.reliable_after(Some(first.seq));

    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].seq, second.seq);
}

#[test]
fn test_memory_event_carries_fields() {
    let sequencer = EventSequencer::new();
    let id = MemoryChangeId::new();
    EventHandler::<MemoryChangedEvent>::handle(
        &sequencer,
        MemoryChangedEvent {
            id,
            origin: AgentOrigin::AutoDream,
            causality: AgentCausality::PreviousTurn,
            changes: vec![
                agent_event::MemoryChange {
                    key: "build-gate".into(),
                    operation: AgentOperation::Promoted,
                    scope: MemoryScope::Project,
                },
                agent_event::MemoryChange {
                    key: "tone".into(),
                    operation: AgentOperation::Updated,
                    scope: MemoryScope::User,
                },
                agent_event::MemoryChange {
                    key: "ledger".into(),
                    operation: AgentOperation::Created,
                    scope: MemoryScope::Auto,
                },
            ],
        },
    );
    let pending = sequencer.sequence_pending_for_test();
    assert!(matches!(
        &pending[0].payload,
        FrontendEvent::MemoryChanged { id: changed_id, origin: MemoryChangeOrigin::AutoDream, causality, changes }
            if changed_id.0 == id.to_string()
                && *causality == MemoryChangeCausality::PreviousTurn
                && changes[0].operation == MemoryOperation::Promoted
                && changes[0].scope == MemoryChangeScope::Project
                && changes[1].scope == MemoryChangeScope::User
                && changes[2].scope == MemoryChangeScope::Auto
    ));
}

#[test]
fn test_tool_event_carries_progress() {
    let sequencer = EventSequencer::new();
    EventHandler::<ToolExecutionEvent>::handle(
        &sequencer,
        ToolExecutionEvent::Progress {
            call_id: "call-1".into(),
            elapsed_secs: 3,
            output_lines: Some(7),
        },
    );
    let pending = sequencer.sequence_pending_for_test();
    assert!(matches!(
        &pending[0].payload,
        FrontendEvent::Acpx { notification }
            if notification.method == AcpxMethod::ToolProgress
    ));
}

#[test]
fn test_notice_event_carries_line() {
    let sequencer = EventSequencer::new();
    EventHandler::<UserNoticeEvent>::handle(
        &sequencer,
        UserNoticeEvent {
            message: "check config".into(),
        },
    );
    let pending = sequencer.sequence_pending_for_test();
    assert!(matches!(
        &pending[0].payload,
        FrontendEvent::SystemLine { text } if text == "check config"
    ));
}

#[test]
fn test_runner_events_defer_sequence() {
    let sequencer = EventSequencer::new();
    EventHandler::<ResponseStreamEvent>::handle(
        &sequencer,
        ResponseStreamEvent::AssistantTextDelta {
            text: "next".into(),
        },
    );

    assert_eq!(sequencer.next_seq(), 0);
    let pending = sequencer.sequence_pending_for_test();
    assert_eq!(pending[0].seq, EventSeq(0));
}

#[test]
fn test_status_replay_keeps_latest() {
    let sequencer = EventSequencer::new();
    sequencer.enqueue_reliable(status("child", 1));
    sequencer.enqueue_reliable(status("child", 2));
    let pending = sequencer.sequence_pending_for_test();
    assert_eq!(pending.len(), 1);
    assert!(matches!(
        pending[0].payload,
        FrontendEvent::AgentStatus { turn: 2, .. }
    ));

    sequencer.enqueue_reliable(status("child", 3));
    drop(sequencer.sequence_pending_for_test());
    let replay = sequencer.reliable_after(None);
    assert_eq!(replay.len(), 1);
    assert!(matches!(
        replay[0].payload,
        FrontendEvent::AgentStatus { turn: 3, .. }
    ));
}

#[test]
fn test_status_coalescing_preserves_order() {
    let sequencer = EventSequencer::new();
    sequencer.enqueue_reliable(status("first", 1));
    sequencer.enqueue_reliable(line("middle"));
    sequencer.enqueue_reliable(status("first", 2));
    sequencer.enqueue_reliable(status("second", 1));

    let pending = sequencer.sequence_pending_for_test();
    assert!(matches!(
        &pending[0].payload,
        FrontendEvent::SystemLine { text } if text == "middle"
    ));
    assert!(matches!(
        &pending[1].payload,
        FrontendEvent::AgentStatus { agent_id, turn: 2, .. } if agent_id == "first"
    ));
    assert!(matches!(
        &pending[2].payload,
        FrontendEvent::AgentStatus { agent_id, turn: 1, .. } if agent_id == "second"
    ));
}

#[test]
fn test_snapshot_blocks_causal_delta() {
    let sequencer = EventSequencer::new();
    let sequencer_for_load = sequencer.clone();
    let (snapshot_tx, snapshot_rx) = channel();
    let (release_tx, release_rx) = channel();
    let sequencing = spawn(move || {
        sequencer_for_load.sequence_available(|_| {
            snapshot_tx.send(()).expect("snapshot entered");
            release_rx.recv().expect("snapshot released");
            vec![durable("e1", vec![line("user"), line("commit")])]
        })
    });
    snapshot_rx.recv().expect("snapshot started");

    let producer = sequencer.clone();
    let (attempt_tx, attempt_rx) = channel();
    let enqueue = spawn(move || {
        attempt_tx.send(()).expect("enqueue attempted");
        producer.enqueue_ephemeral(line("delta"));
    });
    attempt_rx.recv().expect("producer started");
    release_tx.send(()).expect("release snapshot");

    let durable = sequencing.join().expect("sequencing thread");
    enqueue.join().expect("producer thread");
    let pending = sequencer.sequence_pending_for_test();
    assert_eq!(durable[0].seq, EventSeq(0));
    assert_eq!(durable[1].seq, EventSeq(1));
    assert_eq!(pending[0].seq, EventSeq(2));
}
