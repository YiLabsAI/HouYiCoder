//! Tests for this module.

use crate::InMemoryBackend;
use crate::local_file::LocalFileBackend;
use fs::File;
use fs::OpenOptions;
use houyicoder_context::{
    CheckpointId, CheckpointManifest, ContextBackend, ContextError, Disposition, EventId,
    SessionEvent, SessionId, SessionLogEntry, TurnGroup,
};
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;

fn temp_root() -> PathBuf {
    let dir = env::temp_dir().join(format!("houyicoder_local_window_test_{}", SessionId::new()));
    fs::create_dir_all(&dir).expect("create temp root");
    dir
}

fn evt(session: SessionId, id: EventId, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id,
        session,
        ts: 0,
        prev_hash: None,
        event: kind,
    }
}

/// A forward window from offset 0 returns all complete lines + a
/// next_offset past the last line + the byte total. Line-aligned: a
/// window from a mid-line offset skips the partial first line.
#[test]
fn test_range_read_aligns_newlines() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    for i in 0..5 {
        pollster::block_on(b.append(evt(
            s,
            EventId::new(),
            SessionEvent::UserInput {
                text: format!("line-{i}"),
            },
        )))
        .unwrap();
    }
    let total = b.log_size(s);
    let r = b.read_log_range(s, 0, 1 << 20);
    assert_eq!(r.lines.len(), 5, "all 5 lines from offset 0: {:?}", r.lines);
    assert_eq!(r.bytes_total, total);
    assert!(r.next_offset <= total, "next_offset past the last line");
    assert!(r.lines.iter().any(|(_, t)| t.contains("line-4")));
    // From a mid-line offset: the partial first line is dropped.
    let one = r.lines[1].0;
    let mid = one + 3;
    let r2 = b.read_log_range(s, mid, 1 << 20);
    assert!(
        r2.lines
            .iter()
            .all(|(_, t)| t.contains("line-2") || t.contains("line-3") || t.contains("line-4")),
        "partial first line dropped, rest complete: {:?}",
        r2.lines
    );
    fs::remove_dir_all(&root).ok();
}

/// The reverse reader yields lines newest-first with their byte offsets +
/// None next_from at BOF.
#[test]
fn test_reverse_read_newest_first() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    for i in 0..5 {
        pollster::block_on(b.append(evt(
            s,
            EventId::new(),
            SessionEvent::UserInput {
                text: format!("ev-{i}"),
            },
        )))
        .unwrap();
    }
    let total = b.log_size(s);
    let r = b.read_lines_reverse(s, total, 1 << 20);
    assert_eq!(r.next_from, None, "BOF reached");
    assert_eq!(r.lines.len(), 5, "all 5 lines: {:?}", r.lines);
    assert!(
        r.lines[0].1.contains("ev-4"),
        "newest first: {:?}",
        r.lines[0]
    );
    assert!(
        r.lines[4].1.contains("ev-0"),
        "oldest last: {:?}",
        r.lines[4]
    );
    assert!(
        r.lines[0].0 > r.lines[4].0,
        "offsets descend as we go newer"
    );
    fs::remove_dir_all(&root).ok();
}

/// A multi-byte UTF-8 sequence is not corrupted: the window is split on
/// terminators only, so no split falls inside it, no U+FFFD.
#[test]
fn test_reverse_read_multibyte_safe() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    let body = "边界测试 UTF-8 安全性 🦀".to_string();
    pollster::block_on(b.append(evt(
        s,
        EventId::new(),
        SessionEvent::AssistantMessage {
            text: body.clone(),
            thinking: None,
        },
    )))
    .unwrap();
    let total = b.log_size(s);
    let r = b.read_lines_reverse(s, total, 1 << 20);
    assert_eq!(r.lines.len(), 1);
    let text = &r.lines[0].1;
    assert!(
        text.contains("🦀"),
        "multibyte char intact (no U+FFFD): {text}"
    );
    assert!(text.contains("边界测试"));
    fs::remove_dir_all(&root).ok();
}

/// An in-memory backend has no on-disk log, so the windowed reads return
/// empty defaults (the trait's default impls). Covers the default bodies.
#[test]
fn test_in_memory_default_reads() {
    let b = InMemoryBackend::new();
    let s = SessionId::new();
    let r = b.read_log_range(s, 0, 1024);
    assert!(r.lines.is_empty(), "InMemoryBackend range default is empty");
    assert_eq!(r.bytes_total, 0);
    let r2 = b.read_lines_reverse(s, 0, 1024);
    assert!(
        r2.lines.is_empty(),
        "InMemoryBackend reverse default is empty"
    );
    assert_eq!(r2.next_from, None);
}

/// Error paths: a cold session (no log file) returns empty + zero total;
/// an offset past EOF returns empty with the total set. These cover the
/// early-return branches the happy-path tests don't hit.
#[test]
fn test_range_read_error_paths() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    // Cold session: no log file -> empty default, total 0.
    let r = b.read_log_range(s, 0, 1024);
    assert!(r.lines.is_empty(), "cold session: no lines");
    assert_eq!(r.bytes_total, 0, "cold session: zero total");
    // Append one event, then read past EOF.
    pollster::block_on(b.append(evt(
        s,
        EventId::new(),
        SessionEvent::UserInput { text: "x".into() },
    )))
    .unwrap();
    let total = b.log_size(s);
    let past = b.read_log_range(s, total + 100, 1024);
    assert!(past.lines.is_empty(), "past EOF: no lines");
    assert_eq!(past.bytes_total, total, "past EOF: total still set");
    // Reverse read on a cold session also returns empty.
    let cold_session = SessionId::new();
    let rev = b.read_lines_reverse(cold_session, 0, 1024);
    assert!(rev.lines.is_empty(), "cold reverse: no lines");
    assert_eq!(rev.next_from, None);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn test_log_size_round_trips() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    assert_eq!(b.log_size(s), 0, "cold session sizes 0 (no log file)");
    pollster::block_on(b.append(evt(
        s,
        EventId::new(),
        SessionEvent::UserInput { text: "a".into() },
    )))
    .unwrap();
    pollster::block_on(b.append(evt(
        s,
        EventId::new(),
        SessionEvent::UserInput { text: "b".into() },
    )))
    .unwrap();
    assert!(b.log_size(s) > 0, "appended log sizes positive");
    let events = b.read_log(s).expect("read_log ok");
    assert_eq!(events.len(), 2, "both events round-trip in append order");
    assert!(
        matches!(events[0].event, SessionEvent::UserInput { ref text } if text == "a"),
        "first event is the first appended"
    );
    fs::remove_dir_all(&root).ok();
}

/// Appending to a session whose directory is named in the other spelling
/// lands in that directory: the log path resolves through the store's
/// spelling rule, so a writer joins the log a reader would read instead of
/// opening a second directory for one session. Joining the id string
/// unexamined writes a log no scan pairs with its descriptor.
#[test]
fn test_append_lands_legacy_dir() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    let legacy = root.join(s.ulid_name());
    fs::create_dir_all(&legacy).expect("create legacy dir");
    fs::write(legacy.join("log.jsonl"), "").expect("seed log");

    let id = EventId::new();
    pollster::block_on(b.append(evt(
        s,
        id,
        SessionEvent::UserInput {
            text: "into the legacy dir".into(),
        },
    )))
    .unwrap();

    let written = fs::read_to_string(legacy.join("log.jsonl")).expect("read legacy log");
    assert!(
        written.contains("into the legacy dir"),
        "the event lands in the directory the store already holds"
    );
    assert!(
        !root.join(s.to_string()).exists(),
        "one session does not get a second directory"
    );
    fs::remove_dir_all(&root).ok();
}

/// The tolerant read parses good lines + skips + counts bad ones, so one
/// corrupt line does not blank the search snapshot. The strict replay
/// path stays separate (it errors on the bad line) -- two paths, not one.
#[test]
fn test_tolerant_read_skips_corrupt() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    pollster::block_on(b.append(evt(
        s,
        EventId::new(),
        SessionEvent::UserInput {
            text: "good".into(),
        },
    )))
    .unwrap();
    // Inject a corrupt line directly into the log (bad JSON).
    let mut f = OpenOptions::new()
        .append(true)
        .open(b.log_path(s))
        .expect("open log");
    f.write_all(b"not valid json\n").expect("write corrupt");
    let read = b.read_log_lenient(s);
    assert_eq!(read.events.len(), 1, "the good event parses through");
    assert_eq!(read.skipped, 1, "the corrupt line is skipped + counted");
    // The strict path still errors (the two paths are separate).
    assert!(
        b.read_log(s).is_err(),
        "strict replay errors on the corrupt line; tolerant does not"
    );
    fs::remove_dir_all(&root).ok();
}

/// A duplicate event id is a no-op append (main-chain dedup), so the log
/// holds one event + the seen set short-circuits the second write. Covers
/// the dedup return branch.
#[test]
fn test_append_dedup_is_noop() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    let id = EventId::new();
    let event = evt(
        s,
        id,
        SessionEvent::UserInput {
            text: "once".into(),
        },
    );
    pollster::block_on(b.append(event.clone())).unwrap();
    // Same id again: no-op, no second line written.
    pollster::block_on(b.append(event)).unwrap();
    let events = b.read_log(s).expect("read_log ok");
    assert_eq!(events.len(), 1, "duplicate id deduped, one event on disk");
    assert_eq!(
        b.dedup_read_count(),
        1,
        "seen set built once, not per append"
    );
    fs::remove_dir_all(&root).ok();
}

/// A checkpoint round-trips through the on-disk JSON + lists back. Drives
/// the async trait wrappers (write/read/list) + the sync helpers they
/// delegate to.
#[test]
fn test_local_checkpoint_round_trips() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    let last = EventId::new();
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session: s,
        last_event: last,
        summary: Some("head summarized".into()),
        plan: vec![TurnGroup {
            turn_id: last,
            disposition: Disposition::Verbatim,
            event_ids: vec![last],
        }],
        ts: 0,
    };
    let id = pollster::block_on(b.write_checkpoint(manifest.clone())).unwrap();
    assert_eq!(id, manifest.id);
    let back = pollster::block_on(b.read_checkpoint(manifest.id)).unwrap();
    assert_eq!(back.summary, manifest.summary);
    let list = pollster::block_on(b.list_checkpoints(s)).unwrap();
    assert_eq!(list, vec![manifest.id]);
    // A missing checkpoint surfaces NotFound, not a bare IO error.
    let miss = pollster::block_on(b.read_checkpoint(CheckpointId::new()));
    assert!(
        matches!(miss, Err(ContextError::NotFound)),
        "missing -> NotFound"
    );
    fs::remove_dir_all(&root).ok();
}

/// A checkpoint write goes to a temp sibling, fsyncs, then renames, so a
/// reader sees the previous manifest or the complete new one and never a
/// partial file. Once the call returns the temp sibling is gone and the
/// manifest reads back on a fresh backend.
#[test]
fn test_checkpoint_atomic_write() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let session = SessionId::new();
    let manifest = CheckpointManifest {
        id: CheckpointId::new(),
        session,
        last_event: EventId::new(),
        summary: Some("folded".into()),
        plan: vec![TurnGroup {
            turn_id: EventId::new(),
            disposition: Disposition::Summarized,
            event_ids: vec![EventId::new()],
        }],
        ts: 0,
    };
    let mid = manifest.id;
    pollster::block_on(b.write_checkpoint(manifest.clone())).unwrap();
    let cp_path = root
        .join(format!("{session}"))
        .join("checkpoints")
        .join(format!("{mid}.json"));
    assert!(cp_path.exists(), "checkpoint file present");
    assert!(
        !cp_path.with_extension("tmp").exists(),
        "no temp sibling lingers after write_checkpoint"
    );
    let b2 = LocalFileBackend::new(root.clone());
    let back = pollster::block_on(b2.read_checkpoint(mid)).unwrap();
    assert_eq!(
        back.summary,
        Some("folded".into()),
        "checkpoint round-trips"
    );
    fs::remove_dir_all(&root).ok();
}

/// A session's event log replays round-trip across a restart: events
/// appended on one LocalFileBackend instance re-materialize in replay
/// order on a fresh instance at the same root. The durable log is the
/// source of truth the turn-group assembly re-reads each turn, so a
/// resume that cannot replay is a brick. The manifest builder and
/// apply_manifest both read this path after a restart.
#[test]
fn test_replay_session_round_trips() {
    let root = temp_root();
    let s = SessionId::new();
    let id0 = EventId::new();
    let id1 = EventId::new();
    let id2 = EventId::new();
    let events = vec![
        evt(
            s,
            id0,
            SessionEvent::UserInput {
                text: "task".into(),
            },
        ),
        evt(
            s,
            id1,
            SessionEvent::AssistantMessage {
                text: "response".into(),
                thinking: None,
            },
        ),
        evt(
            s,
            id2,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "bash".into(),
                input: serde_json::json!({}),
            },
        ),
    ];
    {
        let b = LocalFileBackend::new(root.clone());
        for ev in &events {
            pollster::block_on(b.append(ev.clone())).unwrap();
        }
        assert_eq!(
            pollster::block_on(b.replay(s)).unwrap().len(),
            events.len(),
            "in-instance replay matches"
        );
    }
    // A fresh backend over the same root replays the persisted log.
    let b2 = LocalFileBackend::new(root.clone());
    let replayed = pollster::block_on(b2.replay(s)).unwrap();
    assert_eq!(
        replayed.len(),
        events.len(),
        "replay across restart recovers every event"
    );
    assert_eq!(replayed[0].id, id0, "replay preserves event order + ids");
    assert_eq!(replayed[2].id, id2);
    assert_eq!(
        replayed[1].event,
        SessionEvent::AssistantMessage {
            text: "response".into(),
            thinking: None,
        },
        "replay preserves the event kind + payload"
    );
    fs::remove_dir_all(&root).ok();
}

/// A window that lands mid-line with no newline in the read returns no
/// complete lines + advances next_offset past the partial content. Covers
/// the no-newline-in-window early return + the empty-lines next_offset
/// branch in read_log_range.
#[test]
fn test_range_read_partial_advances() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    // One long line with no newline.
    let body = "x".repeat(200);
    pollster::block_on(b.append(evt(
        s,
        EventId::new(),
        SessionEvent::UserInput { text: body },
    )))
    .unwrap();
    let total = b.log_size(s);
    // Read a tiny window from the middle: no newline -> no complete lines.
    let r = b.read_log_range(s, 50, 16);
    assert!(r.lines.is_empty(), "no complete line in a partial window");
    assert_eq!(r.bytes_total, total);
    assert!(
        r.next_offset > 50,
        "next_offset advances past the partial read"
    );
    fs::remove_dir_all(&root).ok();
}

/// A reverse read of a single line with no trailing newline yields its
/// content as the log's one line: the window holds no terminator, and
/// nothing above the window ends it either.
#[test]
fn test_reverse_read_no_newline() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    let body = "single line no newline".to_string();
    pollster::block_on(b.append(evt(
        s,
        EventId::new(),
        SessionEvent::AssistantMessage {
            text: body.clone(),
            thinking: None,
        },
    )))
    .unwrap();
    let total = b.log_size(s);
    let r = b.read_lines_reverse(s, total, 1 << 20);
    assert_eq!(r.next_from, None, "BOF reached");
    assert_eq!(r.lines.len(), 1, "the lineless content yields one row");
    assert!(
        r.lines[0].1.contains("single line"),
        "content intact: {}",
        r.lines[0].1
    );
    fs::remove_dir_all(&root).ok();
}

/// A reverse read of an empty log file (file exists, size 0) returns
/// empty + None. Covers the size==0 early return distinct from the
/// no-file (cold session) path.
#[test]
fn test_reverse_read_empty_file() {
    let root = temp_root();
    let b = LocalFileBackend::new(root.clone());
    let s = SessionId::new();
    // Touch the log file so it exists with size 0 (not a cold session).
    let log = b.log_path(s);
    fs::create_dir_all(log.parent().unwrap()).expect("create session dir");
    File::create(&log).expect("create empty log");
    let r = b.read_lines_reverse(s, 0, 1 << 20);
    assert!(r.lines.is_empty(), "empty file yields no lines");
    assert_eq!(r.next_from, None, "BOF (size 0)");
    // Forward range on the empty file is also empty.
    let fwd = b.read_log_range(s, 0, 1 << 20);
    assert!(fwd.lines.is_empty());
    assert_eq!(fwd.bytes_total, 0);
    fs::remove_dir_all(&root).ok();
}
