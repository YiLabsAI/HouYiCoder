//! Pins for the session-log snapshot: the durable log folded through the
//! same mapping the live push uses, the byte-window reads on a real backend,
//! and the offset index. The acceptance guarantees (render parity, multibyte
//! safety, bounded window and index, a row surviving a window that starts
//! mid-turn) live here, not in the module that implements the port.

use super::*;
use crate::session::history::{
    LOOKBACK_STEP_BYTES, MAX_READABLE_LINE_BYTES, log_index::INDEX_WALK_MAX_BYTES,
};
use houyicoder_async::PFut;
use houyicoder_context::{
    BlockHash, CheckpointId, CheckpointManifest, ContextBackend, EventId, LenientRead, LogIndex,
    LogIndexEntry, LogIndexHeader, LogRangeRead, ReverseRead, SAMPLE_INTERVAL_BYTES, SessionEvent,
    session_class::{INDEX_FILE, LOG_FILE},
};
use houyicoder_memory::LocalFileBackend;
use houyicoder_protocol::envelope::{DurableEventId, EventSeq};
use houyicoder_session::SessionStore;
use houyicoder_tui::records::TranscriptLine;
use houyicoder_tui::row_id::RowId;
use houyicoder_tui::transcript::{
    SequencedFrame, transcript_from_frames, transcript_from_frames_at,
};
use std::env::temp_dir;
use std::env::var;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::metadata;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::fs::write;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;
use tokio::runtime::Runtime;

fn ev(event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event,
    }
}

/// A bash call and its result yield a Tool chip row followed by a
/// result row whose body is the raw stdout. The parity guarantee + the
/// footprint source (body stored once).
#[test]
fn test_bash_renders_stdout_body() {
    let events = &[
        ev(SessionEvent::ToolCall {
            call_id: "c1".into(),
            tool: "bash".into(),
            input: serde_json::json!({"command": "echo hi"}),
        }),
        ev(SessionEvent::tool_result(
            "c1".to_string(),
            serde_json::json!({"stdout": "hi\nthere", "exitCode": 0}),
        )),
    ];
    let lines = SessionLogSnapshot::render_events(events);
    assert!(lines.len() >= 2, "call + result rows: {lines:?}");
    let body = match &lines[1] {
        TranscriptLine::Tool { body, .. } => body.clone(),
        other => panic!("expected result row, got {other:?}"),
    };
    assert!(body.contains("hi"), "stdout in body: {body}");
    assert!(body.contains("there"), "full stdout in body: {body}");
}

/// TurnAborted must be visible in the snapshot. The shared mapping
/// closes the drift structurally.
#[test]
fn test_turn_aborted_visible_snapshot() {
    let events = &[ev(SessionEvent::TurnAborted {
        reason: "user escape".into(),
    })];
    let lines = SessionLogSnapshot::render_events(events);
    let text = match &lines[..] {
        [TranscriptLine::User(s)] => s.clone(),
        other => panic!("expected one user notice row, got {other:?}"),
    };
    assert!(
        text.contains("interrupted"),
        "TurnAborted notice in the snapshot: {text}"
    );
    assert!(
        text.contains("user escape"),
        "the abort reason carries through: {text}"
    );
}

/// The durable record reaches the snapshot and closes its turn there, named
/// by its own durable identity. Without the acpx mapping the snapshot
/// carries no record at all, so the turn it closes stays open and no summary
/// row is derived.
#[test]
fn test_snapshot_record_row() {
    let closing = EventId::new();
    let events = &[
        ev(SessionEvent::UserInput { text: "one".into() }),
        ev(SessionEvent::Reasoning {
            text: "weighing".into(),
        }),
        ev(SessionEvent::AssistantMessage {
            text: "first answer".into(),
            thinking: None,
        }),
        ev_session(
            SessionId::new(),
            closing,
            SessionEvent::RunCompleted { ms: Some(4_000) },
        ),
        ev(SessionEvent::UserInput { text: "two".into() }),
        ev(SessionEvent::AssistantMessage {
            text: "second answer".into(),
            thinking: None,
        }),
    ];
    let rows: Vec<(Option<u64>, String)> = SessionLogSnapshot::render_events(events)
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::ThoughtFor { ms, turn_id, .. } => Some((*ms, turn_id.to_string())),
            _ => None,
        })
        .collect();
    assert_eq!(
        rows,
        vec![(Some(4_000), format!("e{closing}.1"))],
        "the record closes the first turn and names the row by its durable identity"
    );
}

/// The frames of one event take their slots in the canonical frame order:
/// the session frame first, the notification frame second. An event whose
/// only frame comes from map_acpx_notification still names that frame at the
/// notification slot — the frame's category decides the slot, not how many
/// frames the event produced.
#[test]
fn test_named_frames_slot_sequence() {
    let input = EventId::new();
    let mid = EventId::new();
    let closing = EventId::new();
    let events = [
        ev_session(
            SessionId::new(),
            input,
            SessionEvent::UserInput { text: "go".into() },
        ),
        ev_session(
            SessionId::new(),
            mid,
            SessionEvent::MidTurnInput {
                text: "queued".into(),
                pending_input_id: None,
            },
        ),
        ev_session(
            SessionId::new(),
            closing,
            SessionEvent::RunCompleted { ms: Some(1_000) },
        ),
    ];
    let (_frames, names) = SessionLogSnapshot::named_frames(events.iter());
    let entry = |id: EventId, slot: u8| RowId::Durable {
        event: DurableEventId(id.to_string()),
        slot,
    };
    assert_eq!(
        names,
        vec![
            entry(input, 0),
            entry(mid, 0),
            entry(mid, 1),
            entry(closing, 1)
        ]
    );
}

/// The two read paths name one durable content alike: the summary row
/// the snapshot derives from log events and the row the live fold derives
/// from stamped frames of the same events carry the same identity, at the
/// notification slot for a record-closed turn.
#[test]
fn test_snapshot_live_names_agree() {
    let closing = EventId::new();
    let events = [
        ev(SessionEvent::UserInput { text: "one".into() }),
        ev(SessionEvent::Reasoning {
            text: "weighing".into(),
        }),
        ev_session(
            SessionId::new(),
            closing,
            SessionEvent::RunCompleted { ms: Some(4_000) },
        ),
    ];
    let row_name = |lines: &[TranscriptLine]| -> Option<RowId> {
        lines.iter().find_map(|l| match l {
            TranscriptLine::ThoughtFor { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        })
    };
    let snapshot_row = row_name(&SessionLogSnapshot::render_events(events.iter()))
        .expect("the record closes the turn in the snapshot");
    let mut log: Vec<SequencedFrame> = Vec::new();
    for entry in &events {
        let id = DurableEventId(entry.id.to_string());
        for frame in SessionLogSnapshot::frames_of(&entry.event)
            .into_iter()
            .flatten()
        {
            log.push(SequencedFrame {
                seq: Some(EventSeq(1)),
                event_id: Some(id.clone()),
                frame,
            });
        }
    }
    let (live_rows, _) = transcript_from_frames_at(&log, 0, 0..log.len(), true, None);
    let live_row = row_name(&live_rows.lines).expect("the live fold closes the same turn");
    let expected = RowId::Durable {
        event: DurableEventId(closing.to_string()),
        slot: 1,
    };
    assert_eq!(
        snapshot_row, expected,
        "the snapshot names the row by the closing entry"
    );
    assert_eq!(
        live_row, expected,
        "the live path gives the same row the same name"
    );
}

/// Source parity on a log whose newest turn the log records as ended: the
/// live read and the snapshot read render the same lines. The parity is
/// scoped to that log shape, because the two readers are told different
/// things about the newest turn (the live read is told it is closed, the
/// snapshot read is told it is open, and no reader may share a fact the
/// other process owns).
#[test]
fn test_live_snapshot_parity() {
    let events = &[
        ev(SessionEvent::UserInput { text: "one".into() }),
        ev(SessionEvent::Reasoning {
            text: "weighing".into(),
        }),
        ev(SessionEvent::AssistantMessage {
            text: "first answer".into(),
            thinking: None,
        }),
        ev(SessionEvent::RunCompleted { ms: Some(4_000) }),
    ];
    let mut live = Vec::new();
    for event in events {
        live.extend(
            SessionLogSnapshot::frames_of(&event.event)
                .into_iter()
                .flatten(),
        );
    }
    let live_rows: Vec<TranscriptLine> = transcript_from_frames(&live, 0..live.len(), false);
    let snapshot_rows = SessionLogSnapshot::render_events(events);
    assert_eq!(
        live_rows.iter().map(|l| l.render()).collect::<Vec<_>>(),
        snapshot_rows.iter().map(|l| l.render()).collect::<Vec<_>>(),
        "a turn the log records as ended renders the same either way"
    );
}

/// The one turn shape the two readers answer differently: a turn the log
/// carries no record for. The live read is told the newest turn is closed,
/// so it derives that turn's summary row without a duration; the snapshot
/// reads the log of a run that may still be writing it, so the turn stays
/// open and no row is derived, rather than a row claiming an end the log
/// does not record.
#[test]
fn test_open_turn_differs() {
    let events = &[
        ev(SessionEvent::UserInput { text: "one".into() }),
        ev(SessionEvent::Reasoning {
            text: "weighing".into(),
        }),
        ev(SessionEvent::AssistantMessage {
            text: "answer".into(),
            thinking: None,
        }),
    ];
    let mut live = Vec::new();
    for event in events {
        live.extend(
            SessionLogSnapshot::frames_of(&event.event)
                .into_iter()
                .flatten(),
        );
    }
    let live_rows: Vec<String> = transcript_from_frames(&live, 0..live.len(), false)
        .iter()
        .map(|l| l.render())
        .collect();
    let snapshot_rows: Vec<String> = SessionLogSnapshot::render_events(events)
        .iter()
        .map(|l| l.render())
        .collect();
    assert!(
        live_rows.iter().any(|r| r.contains("Thought")),
        "the closed turn renders its row: {live_rows:?}"
    );
    assert!(
        !snapshot_rows.iter().any(|r| r.contains("Thought")),
        "the unrecorded turn stays open: {snapshot_rows:?}"
    );
}

/// A window whose oldest line falls inside a turn still renders that turn's
/// summary row. The row is written at the frame that closed the turn, and
/// the fold that writes it reads the frame that opened the turn, so a
/// window carrying the end but not the beginning dropped the row: scrolling
/// into a long turn lost it until the window reached the user message.
#[test]
fn test_window_mid_turn_row() {
    let session = SessionId::new();
    let mut events: Vec<SessionLogEntry> = vec![ev_session(
        session,
        EventId::new(),
        SessionEvent::UserInput {
            text: "start".into(),
        },
    )];
    for i in 0..6 {
        events.push(ev_session(
            session,
            EventId::new(),
            SessionEvent::Reasoning {
                text: format!("step {i}"),
            },
        ));
    }
    events.push(ev_session(
        session,
        EventId::new(),
        SessionEvent::RunCompleted { ms: Some(7_000) },
    ));
    let (snap, _s, root) = bridge_with_log(&events);
    let mut steps = 0;
    while !snap.scan_chunk().done && steps < 1000 {
        steps += 1;
    }
    let anchor = snap.byte_at(4).expect("event four is indexed");
    assert!(anchor > 0, "the anchor sits inside the turn");
    let rows = |lines: &[TranscriptLine]| -> Vec<String> {
        lines
            .iter()
            .filter(|l| matches!(l, TranscriptLine::ThoughtFor { .. }))
            .map(|l| l.render())
            .collect()
    };
    let whole = snap.load(1 << 20);
    assert_eq!(rows(&whole.lines).len(), 1, "the whole log derives the row");
    let mid = snap.window(anchor, 1 << 20);
    assert_eq!(
        rows(&mid.lines),
        rows(&whole.lines),
        "a window starting mid-turn derives the same row: {:?}",
        mid.lines
    );
    remove_dir_all(&root).ok();
}

/// What the summary row of a fold says: the reasoning it gathered and
/// the duration its record closed the turn with. None when the fold
/// writes no row.
fn row_facts(lines: &[TranscriptLine]) -> Option<(Option<String>, Option<u64>)> {
    lines.iter().find_map(|l| match l {
        TranscriptLine::ThoughtFor { ms, reasoning, .. } => Some((reasoning.clone(), *ms)),
        _ => None,
    })
}

/// The window's oldest line is the record that closed the turn, so every fact
/// the row summarizes lies ahead of the window. The fold reads back over the
/// turn's own frames before writing the row there: a view that reaches the
/// turn's end shows that turn, rather than a row with nothing in it.
#[test]
fn test_window_at_record_row() {
    let session = SessionId::new();
    let mut events: Vec<SessionLogEntry> = vec![ev_session(
        session,
        EventId::new(),
        SessionEvent::UserInput {
            text: "start".into(),
        },
    )];
    for i in 0..6 {
        events.push(ev_session(
            session,
            EventId::new(),
            SessionEvent::Reasoning {
                text: format!("step {i}"),
            },
        ));
    }
    events.push(ev_session(
        session,
        EventId::new(),
        SessionEvent::RunCompleted { ms: Some(7_000) },
    ));
    let (snap, _s, root) = bridge_with_log(&events);
    let mut steps = 0;
    while !snap.scan_chunk().done && steps < 1000 {
        steps += 1;
    }
    let anchor = snap.byte_at(7).expect("the record is indexed");
    let whole = row_facts(&snap.load(1 << 20).lines).expect("the whole log derives the row");
    let at_record = snap.window(anchor, 1 << 20);
    assert_eq!(
        row_facts(&at_record.lines),
        Some(whole.clone()),
        "a window opening at the record derives the same row: {:?}",
        at_record.lines
    );
    assert_eq!(whole.1, Some(7_000), "the record's duration closes the row");
    assert_eq!(
        whole.0.as_deref(),
        Some("step 0step 1step 2step 3step 4step 5"),
        "the row carries the turn's reasoning, not the window's"
    );
    remove_dir_all(&root).ok();
}

/// A turn longer than one lookback step that carries a mid-turn message. The
/// read back from the closing record meets that message, but the log marks it
/// as delivered into the turn already running, so it opens none: a read that
/// stopped there would pass the fold a window whose oldest frame is the
/// message, and the row would find no frame that opened its turn. The window
/// reaches the turn's opening instead and derives the row the whole log does.
#[test]
fn test_window_long_turn_row() {
    let session = SessionId::new();
    let chunk = "step text ".repeat(400);
    let mut events: Vec<SessionLogEntry> = vec![ev_session(
        session,
        EventId::new(),
        SessionEvent::UserInput {
            text: "start".into(),
        },
    )];
    for i in 0..20 {
        if i == 10 {
            events.push(ev_session(
                session,
                EventId::new(),
                SessionEvent::MidTurnInput {
                    text: "and this".into(),
                    pending_input_id: None,
                },
            ));
        }
        events.push(ev_session(
            session,
            EventId::new(),
            SessionEvent::Reasoning {
                text: format!("{i} {chunk}"),
            },
        ));
    }
    events.push(ev_session(
        session,
        EventId::new(),
        SessionEvent::RunCompleted { ms: Some(9_000) },
    ));
    let (snap, _s, root) = bridge_with_log(&events);
    let mut steps = 0;
    while !snap.scan_chunk().done && steps < 1000 {
        steps += 1;
    }
    let newest = snap.event_count().expect("the index is built") - 1;
    let anchor = snap
        .byte_at(newest)
        .unwrap_or_else(|| panic!("the record is indexed, count {newest}"));
    assert!(
        anchor > LOOKBACK_STEP_BYTES,
        "the turn spans more than one lookback step: {anchor} bytes of log"
    );
    let whole = row_facts(&snap.load(1 << 20).lines).expect("the whole log derives the row");
    let at_record = snap.window(anchor, 1 << 20);
    assert_eq!(
        row_facts(&at_record.lines),
        Some(whole.clone()),
        "a window at the record derives the same row: {:?}",
        at_record.lines
    );
    assert_eq!(whole.1, Some(9_000), "the record's duration closes the row");
    let reasoning = whole.0.expect("the row carries the turn's reasoning");
    assert!(
        reasoning.starts_with("0 "),
        "the row reaches the turn's opening, past the mid-turn message: {} bytes",
        reasoning.len()
    );
    remove_dir_all(&root).ok();
}

/// The same turn read twice, once by the whole-log load and once by a window
/// opening at the turn's own closing record, yields a row with the same name.
/// The name comes from the durable identity of the event the log recorded the
/// turn's end in, not from where the read happened to start: a window-local
/// position shifts with the read, so a name derived from it detaches the
/// expansion state from its row whenever the window slides, and collides with
/// the live frame-index names the resident transcript already carries.
#[test]
fn test_turn_names_durable() {
    let session = SessionId::new();
    let chunk = "step text ".repeat(400);
    // The first turn is short and the second spans several lookback steps, so
    // the lookback behind a window opening at the second turn's record pulls
    // the whole first turn into the fold-ahead prefix. The prefix is context
    // for the fold, never rows of its own: a window read must derive only the
    // window's own turn.
    let first_close = EventId::new();
    let mut events: Vec<SessionLogEntry> = vec![
        ev_session(
            session,
            EventId::new(),
            SessionEvent::UserInput {
                text: "first".into(),
            },
        ),
        ev_session(
            session,
            EventId::new(),
            SessionEvent::Reasoning {
                text: "weighing".into(),
            },
        ),
        ev_session(
            session,
            first_close,
            SessionEvent::RunCompleted { ms: Some(5_000) },
        ),
        ev_session(
            session,
            EventId::new(),
            SessionEvent::UserInput {
                text: "second".into(),
            },
        ),
    ];
    for i in 0..20 {
        events.push(ev_session(
            session,
            EventId::new(),
            SessionEvent::Reasoning {
                text: format!("{i} {chunk}"),
            },
        ));
    }
    let closing = EventId::new();
    events.push(ev_session(
        session,
        closing,
        SessionEvent::RunCompleted { ms: Some(6_000) },
    ));
    let (snap, _s, root) = bridge_with_log(&events);
    let mut steps = 0;
    while !snap.scan_chunk().done && steps < 1000 {
        steps += 1;
    }
    let ids = |lines: &[TranscriptLine]| -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| match l {
                TranscriptLine::ThoughtFor { turn_id, .. } => Some(turn_id.to_string()),
                _ => None,
            })
            .collect()
    };
    let whole = ids(&snap.load(1 << 20).lines);
    assert_eq!(
        whole,
        vec![format!("e{first_close}.1"), format!("e{closing}.1")],
        "the whole log names each row by the event that closed its turn: {whole:?}"
    );
    let newest = snap.event_count().expect("the index is built") - 1;
    let anchor = snap.byte_at(newest).expect("the record is indexed");
    assert!(
        anchor > LOOKBACK_STEP_BYTES,
        "the second turn spans more than one lookback step: {anchor} bytes of log"
    );
    let at_record = ids(&snap.window(anchor, 1 << 20).lines);
    assert_eq!(
        at_record,
        vec![format!("e{closing}.1")],
        "the window read derives only its own turn, named by the closing event: {at_record:?}"
    );
    assert_eq!(
        whole.last(),
        at_record.last(),
        "the whole log and the window name the same turn the same: {whole:?} vs {at_record:?}"
    );
    remove_dir_all(&root).ok();
}

/// A turn whose reasoning alone runs deeper than eight lookback steps, the
/// reach the budget once allowed: the lookback behind a window opening at the
/// closing record must still find the opener, so the window derives the row
/// the whole-log read derives.
/// Without it the fold sees no frame that opened the turn and the record
/// closes nothing — a long turn renders without its summary row in any window
/// that starts at its end, while the whole-log read shows it.
#[test]
fn test_lookback_finds_opener() {
    let session = SessionId::new();
    // Thirty kilobytes per event, twenty events: the turn spans over six
    // hundred kilobytes of log, deeper than eight sixty-four kilobyte steps.
    let chunk = "step text ".repeat(3_000);
    let mut events: Vec<SessionLogEntry> = vec![ev_session(
        session,
        EventId::new(),
        SessionEvent::UserInput {
            text: "start".into(),
        },
    )];
    for i in 0..20 {
        events.push(ev_session(
            session,
            EventId::new(),
            SessionEvent::Reasoning {
                text: format!("{i} {chunk}"),
            },
        ));
    }
    let closing = EventId::new();
    events.push(ev_session(
        session,
        closing,
        SessionEvent::RunCompleted { ms: Some(5_000) },
    ));
    let (snap, _s, root) = bridge_with_log(&events);
    let mut steps = 0;
    while !snap.scan_chunk().done && steps < 1000 {
        steps += 1;
    }
    let newest = snap.event_count().expect("the index is built") - 1;
    let anchor = snap
        .byte_at(newest)
        .unwrap_or_else(|| panic!("the record is indexed, count {newest}"));
    assert!(
        anchor > LOOKBACK_STEP_BYTES * 8,
        "the opener sits deeper than eight lookback steps: {anchor} bytes of log"
    );
    let whole = row_facts(&snap.load(1 << 21).lines).expect("the whole log derives the row");
    let at_record = snap.window(anchor, 1 << 21);
    assert_eq!(
        row_facts(&at_record.lines),
        Some(whole.clone()),
        "the lookback reaches the opener and the window derives the same row: {:?}",
        at_record.lines
    );
    let name = format!("e{closing}.1");
    let window_names: Vec<String> = at_record
        .lines
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::ThoughtFor { turn_id, .. } => Some(turn_id.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        window_names,
        vec![name],
        "the window names the row by the closing event: {window_names:?}"
    );
    remove_dir_all(&root).ok();
}

/// A turn the log carries no completion record for is closed by the message
/// that opens the turn after it, and a window can start exactly at that
/// message. The fold then names the row from frames it read back rather than
/// frames the window holds, and the name must still be the one the whole log
/// gives: the event holding the turn's newest fact, not the message that
/// closed it. Two turns cover the read-back shapes — one holding reasoning
/// alone, and one holding an answer followed by reasoning.
#[test]
fn test_names_durable_without_record() {
    let session = SessionId::new();
    let r1 = EventId::new();
    let r2 = EventId::new();
    let events = vec![
        ev_session(
            session,
            EventId::new(),
            SessionEvent::UserInput {
                text: "first".into(),
            },
        ),
        ev_session(
            session,
            r1,
            SessionEvent::Reasoning {
                text: "weighing".into(),
            },
        ),
        ev_session(
            session,
            EventId::new(),
            SessionEvent::UserInput {
                text: "second".into(),
            },
        ),
        ev_session(
            session,
            EventId::new(),
            SessionEvent::AssistantMessage {
                text: "answer".into(),
                thinking: None,
            },
        ),
        ev_session(
            session,
            r2,
            SessionEvent::Reasoning {
                text: "more".into(),
            },
        ),
        ev_session(
            session,
            EventId::new(),
            SessionEvent::UserInput {
                text: "third".into(),
            },
        ),
    ];
    let (snap, _s, root) = bridge_with_log(&events);
    let mut steps = 0;
    while !snap.scan_chunk().done && steps < 1000 {
        steps += 1;
    }
    let ids = |lines: &[TranscriptLine]| -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| match l {
                TranscriptLine::ThoughtFor { turn_id, .. } => Some(turn_id.to_string()),
                _ => None,
            })
            .collect()
    };
    let whole = ids(&snap.load(1 << 20).lines);
    assert_eq!(
        whole,
        vec![format!("e{r1}.0"), format!("e{r2}.0")],
        "each record-less turn is named by the event holding its newest fact: {whole:?}"
    );
    let at_second = ids(&snap
        .window(snap.byte_at(2).expect("indexed"), 1 << 20)
        .lines);
    assert_eq!(
        at_second, whole,
        "a window starting at the first closing message reads like the whole log: {at_second:?}"
    );
    let at_third = ids(&snap
        .window(snap.byte_at(5).expect("indexed"), 1 << 20)
        .lines);
    assert_eq!(
        at_third,
        vec![format!("e{r2}.0")],
        "a window starting at the second closing message keeps the name: {at_third:?}"
    );
    remove_dir_all(&root).ok();
}

#[test]
fn test_audit_events() {
    assert!(
        map_session_update(&SessionEvent::MetaUser {
            text: "nudge".into()
        })
        .is_none()
    );
    assert!(
        map_session_update(&SessionEvent::TurnStarted {
            turn: 1,
            call_in_turn: 0
        })
        .is_none()
    );
}

/// Build a real LocalFileBackend + SessionStore + SessionLogSnapshot over a
/// temp root, appending the given events. For the real-backend acceptance
/// tests (parity, multibyte, large-log budget) that must exercise the
/// byte-window + reverse-read + index paths on disk, not the mock.
fn bridge_with_log(events: &[SessionLogEntry]) -> (SessionLogSnapshot, SessionId, PathBuf) {
    let root = temp_dir().join(format!(
        "houyi_bridge_acceptance_{}_{}",
        SessionId::new(),
        id()
    ));
    create_dir_all(&root).expect("create temp root");
    let backend = LocalFileBackend::new(root.clone());
    let store = SessionStore::new(Box::new(backend));
    // SessionStore.append drives a tokio Mutex, so it needs a tokio runtime
    // (pollster cannot drive it); block_on a fresh runtime.
    let rt = Runtime::new().expect("test runtime");
    for ev in events {
        rt.block_on(store.append(ev.clone())).expect("append");
    }
    let session = events.first().map(|e| e.session).unwrap_or_default();
    let snap = SessionLogSnapshot::new(Arc::new(store), session);
    (snap, session, root)
}

fn ev_session(session: SessionId, id: EventId, event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id,
        session,
        ts: 0,
        prev_hash: None,
        event,
    }
}

/// Source parity: the whole-log load and the byte-window read render the
/// same lines for the same events. The window path seeks + parses per
/// screen; the load path reads the whole log tolerantly. Both go through
/// the same map_session_update + transcript_from_frames, so the
/// rendered text must match byte-for-byte (the parity guarantee that
/// closes index!=render).
#[test]
fn test_window_matches_load_render() {
    let session = SessionId::new();
    let events: Vec<SessionLogEntry> = (0..5)
        .map(|i| {
            ev_session(
                session,
                EventId::new(),
                SessionEvent::UserInput {
                    text: format!("line {i}"),
                },
            )
        })
        .collect();
    let (snap, _s, root) = bridge_with_log(&events);
    let load = snap.load(1 << 20);
    let win = snap.window(0, 1 << 20);
    let load_text: Vec<String> = load.lines.iter().map(|l| l.render()).collect();
    let win_text: Vec<String> = win.lines.iter().map(|l| l.render()).collect();
    assert_eq!(load_text, win_text, "load vs window render parity");
    assert!(!win_text.is_empty());
    remove_dir_all(&root).ok();
}

/// A multi-byte UTF-8 sequence is preserved across a window boundary: the
/// 64KB-chunk reverse read + the line-aligned forward read never split a
/// multi-byte sequence, so no U+FFFD appears + the content is intact.
#[test]
fn test_window_safe_on_multibyte() {
    let session = SessionId::new();
    let body = "边界测试 UTF-8 安全性 🦀 end".to_string();
    let events = vec![
        ev_session(
            session,
            EventId::new(),
            SessionEvent::AssistantMessage {
                text: body.clone(),
                thinking: None,
            },
        ),
        ev_session(
            session,
            EventId::new(),
            SessionEvent::AssistantMessage {
                text: "second".into(),
                thinking: None,
            },
        ),
    ];
    let (snap, _s, root) = bridge_with_log(&events);
    // Forward window from 0 + reverse tail window both must keep the
    // multibyte char intact (no corruption across chunk edges).
    let fwd = snap.window(0, 1 << 20);
    let rev = snap.tail_window(1 << 20).unwrap();
    let joined_fwd: String = fwd
        .lines
        .iter()
        .map(|l| l.render())
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        joined_fwd.contains('🦀'),
        "forward window keeps the multibyte: {joined_fwd}"
    );
    let joined_rev: String = rev
        .lines
        .iter()
        .map(|l| l.render())
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        joined_rev.contains('🦀'),
        "reverse tail keeps the multibyte: {joined_rev}"
    );
    remove_dir_all(&root).ok();
}

/// The sidecar index the append path maintains answers offsets before any
/// scan runs, and the scan itself completes in a bounded number of chunks:
/// it discovers the history epochs and checks the sidecar once, and neither
/// loops.
#[test]
fn test_scan_bounded_chunks() {
    let session = SessionId::new();
    let events: Vec<SessionLogEntry> = (0..200)
        .map(|i| {
            ev_session(
                session,
                EventId::new(),
                SessionEvent::UserInput {
                    text: format!("ev {i} padding to a few bytes"),
                },
            )
        })
        .collect();
    let (snap, _s, root) = bridge_with_log(&events);
    // The append-maintained sidecar answers before any scan runs.
    assert_eq!(
        snap.byte_at(0),
        Some(0),
        "the sidecar answers the first position"
    );
    assert_eq!(
        snap.event_count(),
        Some(events.len()),
        "the count walks from the sidecar"
    );
    let mut steps = 0u32;
    let progress = loop {
        let p = snap.scan_chunk();
        steps += 1;
        if p.done || steps > 1000 {
            break p;
        }
    };
    assert!(progress.done, "scan completed in {steps} chunks");
    assert!(steps <= 1000, "bounded, no infinite loop ({steps} steps)");
    assert_eq!(
        progress.total_bytes,
        snap.log_size(),
        "progress spans the log"
    );
    assert_eq!(
        progress.scanned_bytes, progress.total_bytes,
        "the scan reached the head"
    );
    remove_dir_all(&root).ok();
}

/// Every position's offset from the sidecar walk is exactly where that
/// record's line starts in the log: the parity the resident offset table
/// used to guarantee, confirmed against the file's own bytes.
#[test]
fn test_byte_at_matches_lines() {
    let session = SessionId::new();
    let events: Vec<SessionLogEntry> = (0..40)
        .map(|i| {
            ev_session(
                session,
                EventId::new(),
                SessionEvent::UserInput {
                    text: format!("ev {i} padded to a few bytes"),
                },
            )
        })
        .collect();
    let (snap, _s, root) = bridge_with_log(&events);
    let text =
        read_to_string(root.join(format!("{session}")).join(LOG_FILE)).expect("read the log back");
    let mut truth: Vec<u64> = Vec::new();
    let mut at = 0u64;
    for line in text.lines() {
        truth.push(at);
        at += line.len() as u64 + 1;
    }
    assert_eq!(truth.len(), events.len(), "one line per appended record");
    for (i, offset) in truth.iter().enumerate() {
        assert_eq!(
            snap.byte_at(i),
            Some(*offset),
            "position {i} lands on its own line"
        );
    }
    assert_eq!(
        snap.byte_at(events.len()),
        None,
        "past the last record there is no offset"
    );
    assert_eq!(snap.event_count(), Some(events.len()));
    remove_dir_all(&root).ok();
}

/// A log wider than one sampling interval holds several index entries, and
/// each position walks from the entry covering it: at an entry's own ordinal
/// the walk is empty, and positions either side of every boundary match the
/// file's own bytes. The log is written without its sidecar, so the scan's
/// one rebuild has to make the queries answer first.
#[test]
fn test_byte_at_crosses_entries() {
    let root = temp_dir().join(format!("houyi_entry_walk_{}_{}", SessionId::new(), id()));
    let session = SessionId::new();
    let dir = root.join(format!("{session}"));
    create_dir_all(&dir).expect("create session dir");
    // About 6000 records of a few hundred bytes each: the log spans several
    // sampling intervals, so the index holds several entries to walk from.
    let events: Vec<SessionLogEntry> = (0..6000)
        .map(|i| {
            ev_session(
                session,
                EventId::new(),
                SessionEvent::UserInput {
                    text: format!("ev {i} padded so one record is a few hundred bytes wide"),
                },
            )
        })
        .collect();
    let mut text = String::new();
    let mut truth: Vec<u64> = Vec::new();
    for ev in &events {
        truth.push(text.len() as u64);
        text.push_str(&serde_json::to_string(ev).expect("serialize event"));
        text.push('\n');
    }
    write(dir.join(LOG_FILE), &text).expect("write the log");

    let backend = LocalFileBackend::new(root.clone());
    let store = SessionStore::new(Box::new(backend));
    let snap = SessionLogSnapshot::new(Arc::new(store), session);
    // A log written without its sidecar answers nothing until the scan runs.
    assert!(snap.byte_at(0).is_none(), "no sidecar yet, no answer");
    assert!(snap.scan_chunk().done, "one chunk covers this log");
    assert_eq!(
        snap.byte_at(0),
        Some(0),
        "the scan's rebuild made queries answer"
    );

    let loaded = LogIndex::load(&dir.join(INDEX_FILE), text.len() as u64, Some(events[0].id))
        .expect("the rebuild published a valid index");
    assert!(
        loaded.entries.len() >= 2,
        "the log spans several entries: {}",
        loaded.entries.len()
    );
    for entry in &loaded.entries {
        let ordinal = usize::try_from(entry.ordinal).expect("ordinal fits");
        assert_eq!(
            snap.byte_at(ordinal),
            Some(truth[ordinal]),
            "an entry answers with its own offset"
        );
        let neighbors = [ordinal + 1, ordinal.saturating_sub(1)];
        for probe in neighbors.into_iter().filter(|p| *p < events.len()) {
            assert_eq!(
                snap.byte_at(probe),
                Some(truth[probe]),
                "position {probe} beside an entry walks to its own line"
            );
        }
    }
    // Positions far from any entry still walk from the entry covering them.
    for i in (0..events.len()).step_by(997) {
        assert_eq!(snap.byte_at(i), Some(truth[i]), "stride position {i}");
    }
    assert_eq!(
        snap.event_count(),
        Some(events.len()),
        "the count walks from the last entry to the log's end"
    );
    remove_dir_all(&root).ok();
}

/// An index whose header checks out but whose entry does not describe the
/// log's bytes is refused: a query degrades to None rather than answering
/// with an offset that names the wrong record or lands inside one.
#[test]
fn test_byte_at_refuses_mismatch() {
    let session = SessionId::new();
    let events: Vec<SessionLogEntry> = (0..3)
        .map(|i| {
            ev_session(
                session,
                EventId::new(),
                SessionEvent::UserInput {
                    text: format!("ev {i}"),
                },
            )
        })
        .collect();
    let (snap, _s, root) = bridge_with_log(&events);
    let index_path = root.join(format!("{session}")).join(INDEX_FILE);
    let header = LogIndexHeader {
        interval: SAMPLE_INTERVAL_BYTES,
        first_event: Some(events[0].id),
        covered_len: snap.log_size(),
    };

    // A valid header over an entry naming an event the log does not hold.
    let foreign = LogIndexEntry {
        event: EventId::new(),
        ordinal: 0,
        offset: 0,
    };
    let mut bytes = header.encode().to_vec();
    bytes.extend_from_slice(&foreign.encode());
    write(&index_path, &bytes).expect("write the foreign index");
    assert!(
        snap.byte_at(0).is_none(),
        "an entry naming another event is refused"
    );
    assert!(
        snap.event_count().is_none(),
        "the count degrades together with its entry"
    );

    // The right event at an offset inside its line: the bytes at the offset
    // are not the record the entry names.
    let shifted = LogIndexEntry {
        event: events[0].id,
        ordinal: 0,
        offset: 3,
    };
    let mut bytes = header.encode().to_vec();
    bytes.extend_from_slice(&shifted.encode());
    write(&index_path, &bytes).expect("write the shifted index");
    assert!(
        snap.byte_at(0).is_none(),
        "an entry pointing inside a record is refused"
    );

    // A scan chunk over a log whose sidecar is corrupt repairs it: the
    // reader refuses the file, the scan's one rebuild replaces it, and the
    // offsets answer again without any caller knowing the file was bad.
    assert!(
        snap.scan_chunk().done,
        "one chunk covers the log the corrupt sidecar sits beside"
    );
    assert_eq!(
        snap.byte_at(0),
        Some(0),
        "the scan's rebuild replaced the corrupt sidecar"
    );
    assert_eq!(
        snap.event_count(),
        Some(events.len()),
        "the count answers from the rebuilt sidecar"
    );
    remove_dir_all(&root).ok();
}

/// Text padding that keeps one small record a few hundred bytes wide.
const SMALL_PAD: usize = 200;

/// Text padding of the one record wider than a read step. The entry check
/// doubles its step from one read step to two, so the width sits between the
/// two: one step cannot hold the line, and the doubled step can.
const WIDE_PAD: usize = 300 * 1024;

/// The second segment of the stale-sidecar fixture: wide enough that the walk
/// from the index's last entry to the log's end crosses more than one window
/// read.
const SEGMENT_TWO_BYTES: u64 = 1400 * 1024;

/// Room the exact fill leaves for its last record, so the padded record stays
/// a few kilobytes wide.
const FILL_GUARD_BYTES: u64 = 2048;

/// One record's JSONL line: the event id it carries, and the line's text with
/// pad filler bytes behind its index label.
fn record_line(session: SessionId, i: usize, pad: usize) -> (EventId, String) {
    let entry = ev_session(
        session,
        EventId::new(),
        SessionEvent::UserInput {
            text: format!("ev {i} {}", "x".repeat(pad)),
        },
    );
    let mut line = serde_json::to_string(&entry).expect("serialize event");
    line.push('\n');
    (entry.id, line)
}

/// Append one small record to the log text, and its id and true offset to the
/// table the assertions check against.
fn push_record(text: &mut String, truth: &mut Vec<(EventId, u64)>, session: SessionId) {
    let (id, line) = record_line(session, truth.len(), SMALL_PAD);
    truth.push((id, text.len() as u64));
    text.push_str(&line);
}

/// Append the bytes text gained since from to the log file, leaving the
/// sidecar beside it untouched.
fn append_tail(log: &Path, text: &str, from: usize) {
    OpenOptions::new()
        .append(true)
        .open(log)
        .expect("reopen the log")
        .write_all(&text.as_bytes()[from..])
        .expect("append to the log");
}

/// A log whose sidecar went stale between two direct writes: the scan's
/// rebuild indexes the first segment, then the second lands past the index's
/// covered length without touching it. The record wider than a read step sits
/// at the second entry's ordinal, so its check has to double the step; the
/// records past the index's end make every walk cross from one window read
/// into the next, and stop exactly at the log's end.
fn stale_sidecar_log() -> (SessionLogSnapshot, Vec<(EventId, u64)>, usize, PathBuf) {
    let root = temp_dir().join(format!("houyi_stale_walk_{}_{}", SessionId::new(), id()));
    let session = SessionId::new();
    let dir = root.join(format!("{session}"));
    create_dir_all(&dir).expect("create session dir");
    let log = dir.join(LOG_FILE);

    let mut text = String::new();
    let mut truth: Vec<(EventId, u64)> = Vec::new();
    while (text.len() as u64) < SAMPLE_INTERVAL_BYTES {
        push_record(&mut text, &mut truth, session);
    }
    // The first record at or past one interval is the one the rebuild
    // samples, so the wide record is placed exactly there.
    let wide_ordinal = truth.len();
    let (wide_id, wide_line) = record_line(session, wide_ordinal, WIDE_PAD);
    truth.push((wide_id, text.len() as u64));
    text.push_str(&wide_line);
    write(&log, &text).expect("write the first segment");
    let segment_one_len = text.len();

    let backend = LocalFileBackend::new(root.clone());
    let store = SessionStore::new(Box::new(backend));
    let snap = SessionLogSnapshot::new(Arc::new(store), session);
    assert!(snap.byte_at(0).is_none(), "no sidecar yet, no answer");
    assert!(snap.scan_chunk().done, "one chunk covers the first segment");
    assert_eq!(
        snap.byte_at(0),
        Some(0),
        "the scan's rebuild indexed the first segment"
    );

    while ((text.len() - segment_one_len) as u64) < SEGMENT_TWO_BYTES {
        push_record(&mut text, &mut truth, session);
    }
    append_tail(&log, &text, segment_one_len);
    (snap, truth, wide_ordinal, root)
}

/// A walk from a stale index entry crosses from one window read into the
/// next: the entry's own check doubles its step over a record wider than a
/// read, the walk counts the records a continuation window holds without
/// re-skipping its first, and the count ends exactly at the log's end.
#[test]
fn test_byte_at_crosses_windows() {
    let (snap, truth, wide_ordinal, root) = stale_sidecar_log();
    let last = truth.len() - 1;
    assert!(
        truth[last].1 - truth[wide_ordinal].1 > MAX_READABLE_LINE_BYTES,
        "the walk from the last entry crosses more than one window read"
    );
    assert_eq!(
        snap.byte_at(wide_ordinal),
        Some(truth[wide_ordinal].1),
        "the wide record answers after its check doubles the step"
    );
    assert_eq!(
        snap.byte_at(wide_ordinal + 1),
        Some(truth[wide_ordinal + 1].1),
        "the walk steps past the entry's own record"
    );
    assert_eq!(
        snap.byte_at(last),
        Some(truth[last].1),
        "the walk crosses window reads to the last record"
    );
    assert_eq!(
        snap.event_count(),
        Some(truth.len()),
        "the count walks across windows to the log's end"
    );
    remove_dir_all(&root).ok();
}

/// Append records until the log's length is exactly target: small records
/// while room remains, then one record padded to land on it.
fn fill_to_exact(
    text: &mut String,
    truth: &mut Vec<(EventId, u64)>,
    session: SessionId,
    target: u64,
) {
    while text.len() as u64 + FILL_GUARD_BYTES < target {
        push_record(text, truth, session);
    }
    let remaining = target - text.len() as u64;
    let (_, base) = record_line(session, truth.len(), 0);
    let pad = usize::try_from(
        remaining
            .checked_sub(base.len() as u64)
            .expect("the fill guard left room for the last record"),
    )
    .expect("the pad fits a length");
    let (id, line) = record_line(session, truth.len(), pad);
    truth.push((id, text.len() as u64));
    text.push_str(&line);
}

/// The walk budget's edge. A log filled to exactly the budget answers with a
/// whole count and a last offset: the walk ends with its cursor at the log's
/// end, which is the one shape where a spent budget still has an answer. One
/// record past the budget answers with nothing, because a count that stops
/// where the budget does would miss it. The exact fill is the subject -- a
/// log comfortably under the budget proves nothing about the edge.
#[test]
#[ignore]
fn test_count_budget_edge() {
    let root = temp_dir().join(format!("houyi_budget_edge_{}_{}", SessionId::new(), id()));
    let session = SessionId::new();
    let dir = root.join(format!("{session}"));
    create_dir_all(&dir).expect("create session dir");
    let log = dir.join(LOG_FILE);

    // One record, so the rebuild indexes exactly one entry at the log's
    // start and every walk from here pays the whole distance.
    let mut text = String::new();
    let mut truth: Vec<(EventId, u64)> = Vec::new();
    push_record(&mut text, &mut truth, session);
    write(&log, &text).expect("write the first segment");
    let first_len = text.len();
    let backend = LocalFileBackend::new(root.clone());
    let store = SessionStore::new(Box::new(backend));
    let snap = SessionLogSnapshot::new(Arc::new(store), session);
    assert!(snap.scan_chunk().done, "one chunk covers this log");

    fill_to_exact(&mut text, &mut truth, session, INDEX_WALK_MAX_BYTES);
    append_tail(&log, &text, first_len);
    assert_eq!(
        snap.event_count(),
        Some(truth.len()),
        "a log ending exactly at the budget counts whole"
    );
    let last = truth.len() - 1;
    assert_eq!(
        snap.byte_at(last),
        Some(truth[last].1),
        "the last record sits inside the budget"
    );

    let over_len = text.len();
    push_record(&mut text, &mut truth, session);
    append_tail(&log, &text, over_len);
    assert!(
        snap.event_count().is_none(),
        "a record past the budget hides from the count"
    );
    assert!(
        snap.byte_at(truth.len() - 1).is_none(),
        "the walk cannot reach past the budget"
    );
    remove_dir_all(&root).ok();
}

/// A backend whose forward window reads stall on command. A stalled read
/// returns no lines and a cursor that does not move -- the shape a failed
/// read folds into -- while still reporting the log's real length, which is
/// what separates a stall from the log's end. One read passes after each
/// arming, so an entry check succeeds and the walk behind it stalls: the
/// refusal under test is the walk's, not a check that never ran.
struct StallingRangeReads {
    inner: LocalFileBackend,
    stall: Arc<AtomicBool>,
    stalled: Arc<AtomicUsize>,
    passes: AtomicUsize,
}

impl StallingRangeReads {
    fn new(root: PathBuf, stall: Arc<AtomicBool>, stalled: Arc<AtomicUsize>) -> Self {
        Self {
            inner: LocalFileBackend::new(root),
            stall,
            stalled,
            passes: AtomicUsize::new(0),
        }
    }
}

impl ContextBackend for StallingRangeReads {
    fn read_log_range(&self, session: SessionId, byte_offset: u64, max_bytes: u64) -> LogRangeRead {
        if self.stall.load(Ordering::Acquire) {
            if self.passes.fetch_add(1, Ordering::AcqRel) > 0 {
                self.stalled.fetch_add(1, Ordering::AcqRel);
                return LogRangeRead {
                    next_offset: byte_offset,
                    bytes_total: self.inner.log_size(session),
                    ..LogRangeRead::default()
                };
            }
        } else {
            self.passes.store(0, Ordering::Release);
        }
        self.inner.read_log_range(session, byte_offset, max_bytes)
    }

    fn append(&self, event: SessionLogEntry) -> PFut<'_, Result<EventId, ContextError>> {
        self.inner.append(event)
    }
    fn read_range(
        &self,
        session: SessionId,
        from: Option<EventId>,
        to: Option<EventId>,
    ) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.read_range(session, from, to)
    }
    fn replay(&self, session: SessionId) -> PFut<'_, Result<Vec<SessionLogEntry>, ContextError>> {
        self.inner.replay(session)
    }
    fn write_checkpoint(
        &self,
        manifest: CheckpointManifest,
    ) -> PFut<'_, Result<CheckpointId, ContextError>> {
        self.inner.write_checkpoint(manifest)
    }
    fn read_checkpoint(
        &self,
        id: CheckpointId,
    ) -> PFut<'_, Result<CheckpointManifest, ContextError>> {
        self.inner.read_checkpoint(id)
    }
    fn list_checkpoints(
        &self,
        session: SessionId,
    ) -> PFut<'_, Result<Vec<CheckpointId>, ContextError>> {
        self.inner.list_checkpoints(session)
    }
    fn block_put(&self, block: Vec<u8>) -> PFut<'_, Result<BlockHash, ContextError>> {
        self.inner.block_put(block)
    }
    fn block_get(&self, hash: &BlockHash) -> PFut<'_, Result<Vec<u8>, ContextError>> {
        self.inner.block_get(hash)
    }
    fn supports_log_windows(&self) -> bool {
        self.inner.supports_log_windows()
    }
    fn log_size(&self, session: SessionId) -> u64 {
        self.inner.log_size(session)
    }
    fn read_log(&self, session: SessionId) -> Result<Vec<SessionLogEntry>, ContextError> {
        self.inner.read_log(session)
    }
    fn read_log_lenient(&self, session: SessionId) -> LenientRead {
        self.inner.read_log_lenient(session)
    }
    fn read_lines_reverse(
        &self,
        session: SessionId,
        from_byte: u64,
        max_bytes: u64,
    ) -> Result<ReverseRead, ContextError> {
        self.inner.read_lines_reverse(session, from_byte, max_bytes)
    }
    fn log_index_floor(&self, session: SessionId, ordinal: u64) -> Option<LogIndexEntry> {
        self.inner.log_index_floor(session, ordinal)
    }
    fn rebuild_log_index(&self, session: SessionId) -> bool {
        self.inner.rebuild_log_index(session)
    }
    fn session_log_root(&self) -> Option<&Path> {
        self.inner.session_log_root()
    }
}

/// A count that cannot reach the log's end refuses rather than answering
/// with what it saw: a stalled read holds a cursor that does not move while
/// the log's real length says bytes remain, and the records past the stall
/// are exactly the ones a stopped count would miss. The refusal clears when
/// the stall does, which is what says the None came from the stall.
#[test]
fn test_count_refuses_stall() {
    let root = temp_dir().join(format!("houyi_stall_{}_{}", SessionId::new(), id()));
    let session = SessionId::new();
    create_dir_all(&root).expect("create temp root");
    let stall = Arc::new(AtomicBool::new(false));
    let stalled = Arc::new(AtomicUsize::new(0));
    let backend = StallingRangeReads::new(root.clone(), Arc::clone(&stall), Arc::clone(&stalled));
    let store = SessionStore::new(Box::new(backend));
    let rt = Runtime::new().expect("test runtime");
    for i in 0..40 {
        rt.block_on(store.append(ev_session(
            session,
            EventId::new(),
            SessionEvent::UserInput {
                text: format!("ev {i} padded so one record is a few hundred bytes wide"),
            },
        )))
        .expect("append");
    }
    let snap = SessionLogSnapshot::new(Arc::new(store), session);
    assert_eq!(
        snap.event_count(),
        Some(40),
        "the warm sidecar counts before any stall"
    );

    stall.store(true, Ordering::Release);
    assert!(
        snap.event_count().is_none(),
        "a stalled walk refuses a count"
    );
    assert!(stalled.load(Ordering::Acquire) > 0, "the walk stalled");

    stall.store(false, Ordering::Release);
    assert_eq!(
        snap.event_count(),
        Some(40),
        "the refusal clears with the stall"
    );
    remove_dir_all(&root).ok();
}

/// Prepare the budget fixture's log. HOUYICODER_LARGE_LOG naming an
/// existing file symlinks that log in and reports the real-log mode;
/// otherwise a generated local-format log just over the threshold is
/// written (~520 events of ~32 KB, the last carrying the needle so the
/// tail window's mapping has to show it).
fn seed_budget_log(log_path: &Path, session: SessionId, needle: &str) -> bool {
    let real_log = var("HOUYICODER_LARGE_LOG").ok();
    if real_log.as_deref().is_some_and(|p| Path::new(p).exists()) {
        #[cfg(unix)]
        symlink(real_log.as_deref().unwrap(), log_path).expect("symlink the large log");
        return true;
    }
    let mut buf: Vec<u8> = Vec::with_capacity(17 * 1024 * 1024);
    for i in 0..520u32 {
        let text = if i == 519 {
            format!("{needle} {}", "x".repeat(32 * 1024))
        } else {
            "x".repeat(32 * 1024)
        };
        let ev = SessionLogEntry {
            id: EventId::new(),
            session,
            ts: i as u64,
            prev_hash: None,
            event: SessionEvent::UserInput { text },
        };
        let mut line = serde_json::to_vec(&ev).expect("serialize event");
        line.push(b'\n');
        buf.extend_from_slice(&line);
    }
    write(log_path, buf).expect("write the fixture log");
    false
}

/// Real-machine budget on a large log: enter (tail_window) actually
/// materializes the mapping (lines > 0 + the tail needle renders), the
/// enter < 300 ms, one window scan < 100 ms, the scan and sidecar rebuild
/// complete, and the resident window + sidecar stay bounded. A real log set
/// through the env var stresses the byte mechanism on a bigger file (its
/// content assertions are skipped: a foreign-format log parses to empty).
#[test]
#[ignore]
fn test_large_log_budget() {
    let root = temp_dir().join(format!("houyi_large_budget_{}_{}", SessionId::new(), id()));
    let session = SessionId::new();
    let session_dir = root.join(format!("{session}"));
    create_dir_all(&session_dir).expect("create session dir");
    let log_path = session_dir.join(LOG_FILE);

    const NEEDLE: &str = "BUDGETNEEDLE";
    let using_real = seed_budget_log(&log_path, session, NEEDLE);
    let backend = LocalFileBackend::new(root.clone());
    let store = SessionStore::new(Box::new(backend));
    let snap = SessionLogSnapshot::new(Arc::new(store), session);

    let total = snap.log_size();
    assert!(
        total > 16 * 1024 * 1024,
        "the large log is over the threshold ({total} bytes)"
    );

    // Enter (tail window): one 256 KB reverse read + parse + project.
    let t0 = Instant::now();
    let tail = snap.tail_window(WINDOW_MAX_BYTES).unwrap();
    let enter_ms = t0.elapsed().as_millis();
    assert!(
        enter_ms < 300,
        "tail_window < 300ms on {total} bytes (took {enter_ms}ms)"
    );
    // Content materialization (the real-mapping mode): the tail window
    // must hold rendered lines + the needle, not be empty. Skipped for a
    // foreign-format real log (mapping parses to nothing there).
    let rendered: String = tail
        .lines
        .iter()
        .map(|l| l.render())
        .collect::<Vec<_>>()
        .join("|");
    if !using_real {
        assert!(
            !tail.lines.is_empty(),
            "tail window materialized lines (not empty):\n{rendered}"
        );
        assert!(
            rendered.contains(NEEDLE),
            "tail window contains the needle (real mapping):\n{}",
            &rendered[..rendered.len().min(400)]
        );
    }
    // The resident window is bounded by WINDOW_MAX_BYTES regardless of log size.
    let window_bytes: usize = tail.lines.iter().map(|l| l.render().len()).sum();
    assert!(
        window_bytes < 1_000_000,
        "resident window bounded ({window_bytes} bytes), not the whole {total}-byte log"
    );

    // One older-window scan (window_before): bounded read + parse + project < 100 ms.
    let older = tail.start_offset;
    let t0 = Instant::now();
    let _scan = snap.window_before(older, WINDOW_MAX_BYTES).unwrap();
    let scan_ms = t0.elapsed().as_millis();
    assert!(scan_ms < 100, "window_before < 100ms (took {scan_ms}ms)");

    // The scan (with the sidecar rebuild in its first chunk): completes in a
    // bounded number of chunks (no freeze -- one chunk per frame in
    // production; here we drain to done).
    let t0 = Instant::now();
    let mut steps = 0u32;
    let progress = loop {
        let p = snap.scan_chunk();
        steps += 1;
        if p.done || steps > 100_000 {
            break p;
        }
    };
    let build_s = t0.elapsed().as_secs_f64();
    assert!(
        progress.done,
        "scan completed in {steps} chunks / {build_s:.1}s"
    );
    // The sidecar index is sampled by byte interval, so it stays small: a
    // header plus one fixed entry per interval, not one slot per event.
    let idx_bytes = metadata(session_dir.join(INDEX_FILE))
        .map(|m| m.len())
        .unwrap_or(0);
    assert!(
        idx_bytes > 0 && idx_bytes < 5_000_000,
        "index bounded ({idx_bytes} bytes), not the {total}-byte log"
    );
    if !using_real {
        assert_eq!(
            snap.event_count(),
            Some(520),
            "the count answers from the rebuilt sidecar"
        );
    }
    eprintln!(
        "large_log_budget: log {total} bytes ({}), enter {enter_ms}ms, scan {scan_ms}ms, index {steps} chunks {build_s:.1}s ({idx_bytes}B)",
        if using_real { "real" } else { "generated" }
    );
    let _removed = remove_dir_all(&root);
}

/// The rows a scroll back reads are the rows the whole-log load renders, in
/// the same order and one window at a time: each window ends where the next
/// older one was asked to end, and every row's text matches. The seam that
/// puts those rows above the resident ones rests on that parity.
#[test]
fn test_scroll_back_parity() {
    let session = SessionId::new();
    let events: Vec<SessionLogEntry> = (0..8)
        .map(|i| {
            ev_session(
                session,
                EventId::new(),
                SessionEvent::UserInput {
                    text: format!("line {i}"),
                },
            )
        })
        .collect();
    let (snap, _s, root) = bridge_with_log(&events);
    let load: Vec<String> = snap
        .load(1 << 20)
        .lines
        .iter()
        .map(|l| l.render())
        .collect();

    let mut walked: Vec<String> = Vec::new();
    let mut windows = 0usize;
    let mut anchor = snap.log_size();
    while anchor > 0 {
        let window = snap.window_before(anchor, 1024).unwrap();
        if window.lines.is_empty() {
            break;
        }
        windows += 1;
        let mut texts: Vec<String> = window.lines.iter().map(|l| l.render()).collect();
        texts.extend(walked);
        walked = texts;
        anchor = window.start_offset;
    }

    assert!(
        windows > 1,
        "the walk crossed more than one window, or this checks nothing"
    );
    assert_eq!(walked, load, "the walk renders the load's rows");
    remove_dir_all(&root).ok();
}
