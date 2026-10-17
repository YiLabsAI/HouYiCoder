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

/// The payload bytes of every frame the window tests push, chosen so a small
/// budget is reached in a few frames.
const PUSH_BYTES: usize = 400;

/// The window budget those tests run with: two pushed frames fit it and a
/// third does not.
const TEST_WINDOW_BYTES: usize = 1024;

/// A sequencer whose window has already dropped its oldest frames, with no
/// client cursor: four durable entries were loaded, and their size left the
/// window holding the newest two. Every frame here traces back to the log, so
/// what the window dropped can be read out of the log again.
fn trimmed_sequencer() -> EventSequencer {
    let sequencer = EventSequencer::with_window(TEST_WINDOW_BYTES);
    sequencer.sequence_available(|_| {
        (0..4)
            .map(|index| durable(&format!("e{index}"), vec![line(&"p".repeat(PUSH_BYTES))]))
            .collect()
    });
    sequencer
}

#[test]
fn test_window_drops_oldest_frames() {
    // Retention is bounded by the bytes the frames carry, not by how many
    // were pushed: a session of any length keeps a window of the same size,
    // holding the newest frames.
    let sequencer = EventSequencer::with_window(TEST_WINDOW_BYTES);
    let mut pushed = Vec::new();
    for _ in 0..20 {
        pushed.push(sequencer.sequence_reliable(line(&"p".repeat(PUSH_BYTES))));
    }

    let retained = sequencer.reliable_after(None);
    assert_eq!(retained.len(), 2, "a window holds what fits its budget");
    assert_eq!(retained[1].seq, pushed[19].seq, "the newest frame is kept");
    assert_eq!(
        retained[0].seq, pushed[18].seq,
        "retention follows the tail"
    );
}

#[test]
fn test_window_keeps_oversized_frame() {
    // The newest frame stays whatever it holds, because the window exists to
    // answer a reconnect and an empty one answers nothing.
    let sequencer = EventSequencer::with_window(TEST_WINDOW_BYTES);
    let oversized = sequencer.sequence_reliable(line(&"p".repeat(4 * TEST_WINDOW_BYTES)));

    let retained = sequencer.reliable_after(None);
    assert_eq!(retained.len(), 1, "one frame outlives the budget it breaks");
    assert_eq!(retained[0].seq, oversized.seq);
}

#[test]
fn test_replay_sends_missed_frames() {
    // A cursor the window still covers is answered from memory. The durable
    // entry this round loads is the size of the frames pushed before it, so
    // adding it to the window evicts the oldest frame the cursor is missing:
    // a delta read back out of the window afterwards would lose that frame,
    // and taking it before the load is what keeps the reconnect whole.
    let sequencer = trimmed_sequencer();
    let filler = "p".repeat(PUSH_BYTES);
    let mut asked = None;
    let replay = sequencer.prepare_replay(Some(EventSeq(1)), |cursor| {
        asked = Some(cursor);
        vec![durable("e2", vec![line(&filler)])]
    });

    let seqs: Vec<u64> = replay.iter().map(|frame| frame.seq.0).collect();
    assert_eq!(asked, Some(4), "the durable cursor is read where it stands");
    assert_eq!(seqs, vec![2, 3, 4], "the delta plus the loaded entry");
}

#[test]
fn test_replay_rereads_below_window() {
    // A cursor below the window floor names a frame that was dropped, so the
    // delta cannot come from memory: the log is sequenced again from its
    // start, which is the history the window was trimmed against.
    let sequencer = trimmed_sequencer();
    let mut asked = None;
    let replay = sequencer.prepare_replay(Some(EventSeq(0)), |cursor| {
        asked = Some(cursor);
        vec![durable("e9", vec![line("from-log"), line("and-more")])]
    });

    let seqs: Vec<u64> = replay.iter().map(|frame| frame.seq.0).collect();
    assert_eq!(asked, Some(0), "the whole log is read again");
    assert_eq!(seqs, vec![4, 5], "every entry comes back, in log order");
}

#[test]
fn test_replay_rereads_without_cursor() {
    // A client with no cursor has seen nothing, so the window tail is not the
    // history it needs.
    let sequencer = trimmed_sequencer();
    let mut asked = None;
    let replay = sequencer.prepare_replay(None, |cursor| {
        asked = Some(cursor);
        vec![durable("e9", vec![line("from-log")])]
    });

    assert_eq!(asked, Some(0), "the whole log is read again");
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].seq, EventSeq(4));
}

/// A notice, a memory change and a live status are pushed by producers at
/// runtime, so no log entry produced them and a client attaching without a
/// cursor cannot be answered from the log alone. Sequencing that log fills the
/// window, so a frame lifted after it has been dropped is a frame no one
/// receives.
#[test]
fn test_replay_keeps_runtime_frames() {
    let sequencer = EventSequencer::with_window(TEST_WINDOW_BYTES);
    sequencer.sequence_reliable(line("notice"));
    let change_id = MemoryChangeId::new();
    EventHandler::<MemoryChangedEvent>::handle(
        &sequencer,
        MemoryChangedEvent {
            id: change_id,
            origin: AgentOrigin::AutoMemory,
            causality: AgentCausality::ThisTurn,
            changes: Vec::new(),
        },
    );
    drop(sequencer.sequence_pending_for_test());
    sequencer.sequence_reliable(status("child", 1));

    let replay = sequencer.prepare_replay(None, |_| {
        vec![durable(
            "e1",
            vec![line(&"p".repeat(4 * TEST_WINDOW_BYTES))],
        )]
    });

    assert!(
        replay.iter().any(
            |frame| matches!(&frame.payload, FrontendEvent::SystemLine { text } if text == "notice")
        ),
        "a runtime notice is held here and nowhere else"
    );
    assert!(
        replay
            .iter()
            .any(|frame| matches!(&frame.payload, FrontendEvent::MemoryChanged { id, .. } if id.0 == change_id.to_string())),
        "a memory change notice is held here and nowhere else"
    );
    assert!(
        replay
            .iter()
            .any(|frame| matches!(&frame.payload, FrontendEvent::AgentStatus { agent_id, .. } if agent_id == "child")),
        "the newest status of an agent outlives the window"
    );
    assert_eq!(replay.len(), 4, "the log entry and the three held frames");
}

/// The same log entry read twice carries the same identity, so a window that
/// kept the first copy beside the second would hold that entry twice and spend
/// the budget of a frame it already has.
#[test]
fn test_reread_replaces_retained_frames() {
    let sequencer = EventSequencer::with_window(TEST_WINDOW_BYTES);
    sequencer.sequence_available(|_| vec![durable("e1", vec![line("first")])]);
    sequencer.prepare_replay(None, |_| vec![durable("e1", vec![line("first")])]);

    let retained = sequencer.reliable_after(None);
    let identities: Vec<&str> = retained
        .iter()
        .filter_map(|frame| frame.event_id.as_ref().map(|id| id.0.as_str()))
        .collect();
    assert_eq!(identities, vec!["e1"], "the re-read replaces the copy");
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
