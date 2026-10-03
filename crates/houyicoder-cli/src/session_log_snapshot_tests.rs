//! Pins for the session-log snapshot: the durable log projected through the
//! same mapping the live push uses, the byte-window reads on a real backend,
//! and the offset index. The acceptance guarantees (render parity, multibyte
//! safety, bounded window and index, a row surviving a window that starts
//! mid-turn) live here, not in the module that implements the port.

use super::*;
use crate::session_history::LOOKBACK_STEP_BYTES;
use houyicoder_context::{EventId, SessionEvent};
use houyicoder_tui::records::TranscriptLine;
use houyicoder_tui::transcript::transcript_from_frames;

fn ev(event: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: 0,
        prev_hash: None,
        event,
    }
}

/// A bash call + result pair projects to a Tool chip row followed by a
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
    let lines = SessionLogSnapshot::project_events(events);
    assert!(lines.len() >= 2, "call + result rows: {lines:?}");
    let body = match &lines[1] {
        TranscriptLine::Tool { body, .. } => body.clone(),
        other => panic!("expected result row, got {other:?}"),
    };
    assert!(body.contains("hi"), "stdout in body: {body}");
    assert!(body.contains("there"), "full stdout in body: {body}");
}

/// TurnAborted must surface in the snapshot. The shared mapping
/// closes the drift structurally; this test pins it.
#[test]
fn test_turn_aborted_visible_snapshot() {
    let events = &[ev(SessionEvent::TurnAborted {
        reason: "user escape".into(),
    })];
    let lines = SessionLogSnapshot::project_events(events);
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
    let rows: Vec<(Option<u64>, String)> = SessionLogSnapshot::project_events(events)
        .iter()
        .filter_map(|l| match l {
            TranscriptLine::ThoughtFor { ms, turn_id, .. } => Some((*ms, turn_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        rows,
        vec![(Some(4_000), format!("e{closing}"))],
        "the record closes the first turn and names the row by its durable identity"
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
    let snapshot_rows = SessionLogSnapshot::project_events(events);
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
    let snapshot_rows: Vec<String> = SessionLogSnapshot::project_events(events)
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
    while !snap.index_chunk().done && steps < 1000 {
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
    std::fs::remove_dir_all(&root).ok();
}

/// What the summary row of a projection says: the reasoning it gathered and
/// the duration its record closed the turn with. None when the projection
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
    while !snap.index_chunk().done && steps < 1000 {
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
    std::fs::remove_dir_all(&root).ok();
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
    while !snap.index_chunk().done && steps < 1000 {
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
    std::fs::remove_dir_all(&root).ok();
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
    while !snap.index_chunk().done && steps < 1000 {
        steps += 1;
    }
    let ids = |lines: &[TranscriptLine]| -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| match l {
                TranscriptLine::ThoughtFor { turn_id, .. } => Some(turn_id.clone()),
                _ => None,
            })
            .collect()
    };
    let whole = ids(&snap.load(1 << 20).lines);
    assert_eq!(
        whole,
        vec![format!("e{first_close}"), format!("e{closing}")],
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
        vec![format!("e{closing}")],
        "the window read derives only its own turn, named by the closing event: {at_record:?}"
    );
    assert_eq!(
        whole.last(),
        at_record.last(),
        "the whole log and the window name the same turn the same: {whole:?} vs {at_record:?}"
    );
    std::fs::remove_dir_all(&root).ok();
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
    while !snap.index_chunk().done && steps < 1000 {
        steps += 1;
    }
    let ids = |lines: &[TranscriptLine]| -> Vec<String> {
        lines
            .iter()
            .filter_map(|l| match l {
                TranscriptLine::ThoughtFor { turn_id, .. } => Some(turn_id.clone()),
                _ => None,
            })
            .collect()
    };
    let whole = ids(&snap.load(1 << 20).lines);
    assert_eq!(
        whole,
        vec![format!("e{r1}"), format!("e{r2}")],
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
        vec![format!("e{r2}")],
        "a window starting at the second closing message keeps the name: {at_third:?}"
    );
    std::fs::remove_dir_all(&root).ok();
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
fn bridge_with_log(
    events: &[SessionLogEntry],
) -> (SessionLogSnapshot, SessionId, std::path::PathBuf) {
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;
    let root = std::env::temp_dir().join(format!(
        "houyi_bridge_acceptance_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let backend = LocalFileBackend::new(root.clone());
    let store = SessionStore::new(Box::new(backend));
    // SessionStore.append drives a tokio Mutex, so it needs a tokio runtime
    // (pollster cannot drive it); block_on a fresh runtime.
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
    for ev in events {
        rt.block_on(store.append(ev.clone())).expect("append");
    }
    let session = events.first().map(|e| e.session).unwrap_or_default();
    let snap = SessionLogSnapshot::new(std::sync::Arc::new(store), session);
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
    std::fs::remove_dir_all(&root).ok();
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
    let rev = snap.tail_window(1 << 20);
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
    std::fs::remove_dir_all(&root).ok();
}

/// The lazy index does not cover the whole log until G completes: byte_at
/// returns None for un-indexed positions, then Some after the build. The
/// full build completes in a bounded number of chunks (no infinite loop).
#[test]
fn test_index_builds_bounded_chunks() {
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
    // Before the build, byte_at is None (index not done).
    assert!(snap.byte_at(0).is_none(), "byte_at None before the build");
    let mut steps = 0u32;
    let progress = loop {
        let p = snap.index_chunk();
        steps += 1;
        if p.done || steps > 1000 {
            break p;
        }
    };
    assert!(progress.done, "index build completed in {steps} chunks");
    assert!(steps <= 1000, "bounded, no infinite loop ({steps} steps)");
    assert!(snap.byte_at(0).is_some(), "byte_at answers after the build");
    assert!(
        snap.event_count().is_some(),
        "event_count answers after the build"
    );
    std::fs::remove_dir_all(&root).ok();
}

/// Real-machine budget on a large log: enter (tail_window) actually
/// materializes the mapping (lines > 0 + the tail needle renders), the
/// enter < 300 ms, one window scan < 100 ms, the full index build
/// completes, and the resident window + index stay bounded. Generates a
/// synthetic local-format log just over the threshold so the mapping is
/// real (a foreign-format log would parse-skip to empty, measuring only
/// the byte mechanism). Set HOUYICODER_LARGE_LOG to a real log path to
/// additionally stress the byte mechanism on a bigger file (mapping may
/// be empty there -- the content assertions are skipped in that mode).
#[test]
#[ignore]
// too_many_lines: a budget benchmark -- setup, measure, assert in one body.
// Splitting obscures the measured region.
#[expect(clippy::too_many_lines, reason = "long by design, kept whole")]
fn test_large_log_budget() {
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;
    let root = std::env::temp_dir().join(format!(
        "houyi_large_budget_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    let session = SessionId::new();
    let session_dir = root.join(format!("{session}"));
    std::fs::create_dir_all(&session_dir).expect("create session dir");
    let log_path = session_dir.join("log.jsonl");

    const NEEDLE: &str = "BUDGETNEEDLE";
    let real_log = std::env::var("HOUYICODER_LARGE_LOG").ok();
    let using_real = real_log
        .as_ref()
        .map(|p| std::path::Path::new(p).exists())
        .unwrap_or(false);
    if using_real {
        #[cfg(unix)]
        std::os::unix::fs::symlink(real_log.as_deref().unwrap(), &log_path)
            .expect("symlink the large log");
    } else {
        // Synthetic local-format log just over the threshold: ~520 events
        // with a ~32 KB body each ~ 16+ MB. The last event carries the
        // needle so the tail window's mapping must surface it.
        let mut buf: Vec<u8> = Vec::with_capacity(17 * 1024 * 1024);
        for i in 0..520u32 {
            let text = if i == 519 {
                format!("{NEEDLE} {}", "x".repeat(32 * 1024))
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
        std::fs::write(&log_path, buf).expect("write synthetic log");
    }
    let backend = LocalFileBackend::new(root.clone());
    let store = SessionStore::new(Box::new(backend));
    let snap = SessionLogSnapshot::new(std::sync::Arc::new(store), session);

    let total = snap.log_size();
    assert!(
        total > 16 * 1024 * 1024,
        "the large log is over the threshold ({total} bytes)"
    );

    // Enter (tail window): one 256 KB reverse read + parse + project.
    let t0 = std::time::Instant::now();
    let tail = snap.tail_window(WINDOW_MAX_BYTES);
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
    let t0 = std::time::Instant::now();
    let _scan = snap.window_before(tail.start_offset, WINDOW_MAX_BYTES);
    let scan_ms = t0.elapsed().as_millis();
    assert!(scan_ms < 100, "window_before < 100ms (took {scan_ms}ms)");

    // Full index build: completes in a bounded number of chunks (no freeze
    // -- one chunk per frame in production; here we drain to done).
    let t0 = std::time::Instant::now();
    let mut steps = 0u32;
    let progress = loop {
        let p = snap.index_chunk();
        steps += 1;
        if p.done || steps > 100_000 {
            break p;
        }
    };
    let build_s = t0.elapsed().as_secs_f64();
    assert!(
        progress.done,
        "full index completed in {steps} chunks / {build_s:.1}s"
    );
    // The offset index size is event-count x 8 bytes, not the log size.
    let idx_bytes = snap.event_count().map(|n| n * 8).unwrap_or(0);
    assert!(
        idx_bytes < 5_000_000,
        "index bounded ({idx_bytes} bytes), not the {total}-byte log"
    );
    eprintln!(
        "large_log_budget: log {total} bytes ({}), enter {enter_ms}ms, scan {scan_ms}ms, index {steps} chunks {build_s:.1}s ({idx_bytes}B)",
        if using_real { "real" } else { "synthetic" }
    );
    let _removed = std::fs::remove_dir_all(&root);
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
        let window = snap.window_before(anchor, 1024);
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
    std::fs::remove_dir_all(&root).ok();
}
