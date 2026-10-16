//! Tests for the shared history reader's typed page reads: a page is a count
//! of turns, the byte budget bounds the walk rather than the page, and a cut
//! turn is reported rather than presented as whole.

use super::*;
use houyicoder_context::{
    ENTRY_LEN, EventId, LogIndex, LogIndexEntry, LogIndexHeader, SAMPLE_INTERVAL_BYTES,
    SessionEvent, SessionLogEntry,
    session_class::{INDEX_FILE, LOG_FILE},
};
use houyicoder_memory::LocalFileBackend;
use houyicoder_session::SessionStore;
use std::env::consts::ARCH;
use std::env::consts::OS;
use std::env::temp_dir;
use std::env::var;
use std::fs::File;
use std::fs::create_dir_all;
use std::fs::metadata;
use std::fs::read;
use std::fs::read_to_string;
use std::fs::remove_dir_all;
use std::fs::write;
use std::io::BufReader;
use std::io::BufWriter;
use std::io::Write;
use std::io::{BufRead, Seek, SeekFrom};
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::id;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use tokio::runtime::Runtime;

/// A cancel flag no read sets: a test drives the read to its end.
fn live() -> AtomicBool {
    AtomicBool::new(false)
}

fn ev(ts: u64, kind: SessionEvent) -> SessionLogEntry {
    SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts,
        prev_hash: None,
        event: kind,
    }
}

/// A session of the given number of turns, each a user input then an answer.
fn session_of(turns: usize) -> (SessionHistory, SessionId, PathBuf) {
    let root = temp_dir().join(format!("houyi_history_page_{}_{}", SessionId::new(), id()));
    create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let rt = Runtime::new().expect("test runtime");
    let session = SessionId::new();
    let mut events = Vec::new();
    for i in 0..turns {
        events.push(SessionLogEntry {
            session,
            ..ev(
                (i as u64) * 1000,
                SessionEvent::UserInput {
                    text: format!("prompt {i}"),
                },
            )
        });
        events.push(SessionLogEntry {
            session,
            ..ev(
                (i as u64) * 1000 + 10,
                SessionEvent::AssistantMessage {
                    text: format!("answer {i}"),
                    thinking: None,
                },
            )
        });
    }
    for event in events {
        rt.block_on(store.append(event)).expect("append");
    }
    let history = SessionHistory::new(Arc::new(store), session);
    (history, session, root)
}

/// A page holds the newest turns and no more, however many the log has.
#[test]
fn test_tail_page_keeps_turns() {
    let (history, _, _) = session_of(10);
    let page = history.turns_before(history.log_size(), 3, PAGE_MAX_BYTES);
    let prompts: Vec<String> = page
        .events
        .iter()
        .filter_map(|e| match &e.entry.event {
            SessionEvent::UserInput { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        prompts,
        vec!["prompt 7", "prompt 8", "prompt 9"],
        "the page keeps the newest turns and starts at a whole turn"
    );
    assert!(!page.oldest_partial, "the oldest turn is whole");
    assert!(
        page.oldest_anchor.is_some(),
        "older turns remain and the anchor points at them"
    );
}

/// Reading older from the page's anchor continues backwards without overlap.
#[test]
fn test_older_page_continues() {
    let (history, _, _) = session_of(10);
    let tail = history.turns_before(history.log_size(), 3, PAGE_MAX_BYTES);
    let older = history.turns_before(
        tail.oldest_anchor.expect("anchor").byte_offset,
        3,
        PAGE_MAX_BYTES,
    );
    let prompts: Vec<String> = older
        .events
        .iter()
        .filter_map(|e| match &e.entry.event {
            SessionEvent::UserInput { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, vec!["prompt 4", "prompt 5", "prompt 6"]);
}

/// The head page is read from the other end: it keeps the session's first
/// turns, and it carries no anchor because nothing sits behind it.
#[test]
fn test_head_page_keeps_first() {
    let (history, _, _) = session_of(10);
    let page = history.head_turns(None, 3, PAGE_MAX_BYTES, &live());
    let prompts: Vec<String> = page
        .events
        .iter()
        .filter_map(|e| match &e.entry.event {
            SessionEvent::UserInput { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        prompts,
        vec!["prompt 0", "prompt 1", "prompt 2"],
        "the head page holds the oldest turns, not the newest"
    );
    assert_eq!(
        page.oldest_anchor, None,
        "nothing older than the head is readable"
    );
    assert!(!page.oldest_partial, "the head starts at a whole turn");
    assert_eq!(page.turn_count(), 3);
}

/// An anchor is checkable: the id at its offset is what says the bytes still
/// describe the turn the anchor named.
#[test]
fn test_anchor_holds_its_turn() {
    let (history, _, _) = session_of(10);
    let page = history.turns_before(history.log_size(), 3, PAGE_MAX_BYTES);
    let anchor = page.oldest_anchor.expect("a page behind the tail");
    assert!(
        history.anchor_holds(anchor),
        "the anchor names the turn at its offset"
    );
    let other_id = TurnAnchor {
        user_input_id: EventId::new(),
        ..anchor
    };
    assert!(
        !history.anchor_holds(other_id),
        "a different id is not that turn"
    );
    let inside_a_line = TurnAnchor {
        byte_offset: anchor.byte_offset + 1,
        ..anchor
    };
    assert!(
        !history.anchor_holds(inside_a_line),
        "an offset inside a line does not open the turn"
    );
}

/// A log smaller than the page is the whole head, and still reports no anchor.
#[test]
fn test_head_page_at_end() {
    let (history, _, _) = session_of(2);
    let page = history.head_turns(None, 5, PAGE_MAX_BYTES, &live());
    assert_eq!(page.turn_count(), 2);
    assert_eq!(page.oldest_anchor, None);
}

/// A cleared session counts only what came after the clear, so its head is the
/// first turn after the event that began the epoch, not the log's first turn.
#[test]
fn test_head_page_after_clear() {
    let root = temp_dir().join(format!(
        "houyi_history_head_clear_{}_{}",
        SessionId::new(),
        id()
    ));
    create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let rt = Runtime::new().expect("test runtime");
    let session = SessionId::new();
    for i in 0..4u64 {
        rt.block_on(store.append(SessionLogEntry {
            session,
            ..ev(
                i * 1000,
                SessionEvent::UserInput {
                    text: format!("before {i}"),
                },
            )
        }))
        .expect("append before");
    }
    let clear = SessionLogEntry {
        session,
        ..ev(5000, SessionEvent::ContextCleared { prior_turn: 4 })
    };
    let clear_id = clear.id;
    rt.block_on(store.append(clear)).expect("append the clear");
    for i in 0..3u64 {
        rt.block_on(store.append(SessionLogEntry {
            session,
            ..ev(
                6000 + i * 1000,
                SessionEvent::UserInput {
                    text: format!("after {i}"),
                },
            )
        }))
        .expect("append after");
    }

    let history = SessionHistory::new(Arc::new(store), session);
    let page = history.head_turns(Some(clear_id), 3, PAGE_MAX_BYTES, &live());
    let prompts: Vec<String> = page
        .events
        .iter()
        .filter_map(|e| match &e.entry.event {
            SessionEvent::UserInput { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        prompts,
        vec!["after 0", "after 1", "after 2"],
        "the head is the history the session counts, not the file"
    );
}

/// An epoch the log does not hold yields no page, so a caller shows nothing
/// rather than the wrong history.
#[test]
fn test_head_page_unknown_epoch() {
    let (history, _, _) = session_of(10);
    let page = history.head_turns(Some(EventId::new()), 3, PAGE_MAX_BYTES, &live());
    assert!(
        page.events.is_empty(),
        "an epoch that cannot be located is not answered with the log's start"
    );
}

/// A first event wider than one read step must still open the head page: the
/// forward walk grows its step rather than returning an empty page.
#[test]
fn test_head_page_wide_first() {
    let root = temp_dir().join(format!(
        "houyi_history_head_wide_{}_{}",
        SessionId::new(),
        id()
    ));
    create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let rt = Runtime::new().expect("test runtime");
    let session = SessionId::new();
    let mut events = vec![SessionLogEntry {
        session,
        ..ev(
            0,
            SessionEvent::AssistantMessage {
                text: "x".repeat(300 * 1024),
                thinking: None,
            },
        )
    }];
    for i in 0..2u64 {
        events.push(SessionLogEntry {
            session,
            ..ev(
                1000 + i * 1000,
                SessionEvent::UserInput {
                    text: format!("prompt {i}"),
                },
            )
        });
    }
    for event in events {
        rt.block_on(store.append(event)).expect("append");
    }
    let history = SessionHistory::new(Arc::new(store), session);
    let page = history.head_turns(None, 2, PAGE_MAX_BYTES, &live());
    assert!(
        page.turn_count() == 2,
        "the wide first event does not hide the turns after it: {}",
        page.turn_count()
    );
}

/// A log smaller than a page yields everything and reports no older anchor.
#[test]
fn test_page_at_log_start() {
    let (history, _, _) = session_of(2);
    let page = history.turns_before(history.log_size(), 5, PAGE_MAX_BYTES);
    assert_eq!(turns_opened(&page.events), 2);
    assert_eq!(page.oldest_anchor, None, "nothing older to read");
    assert!(!page.oldest_partial);
}

/// A single event wider than one read step must still be found: the reverse
/// read returns nothing until its budget can reach a line's start, so the step
/// has to grow rather than the page giving up.
#[test]
fn test_page_reads_wide_event() {
    let root = temp_dir().join(format!("houyi_history_wide_{}_{}", SessionId::new(), id()));
    create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let rt = Runtime::new().expect("test runtime");
    let session = SessionId::new();
    let mut events = Vec::new();
    for i in 0..2u64 {
        events.push(SessionLogEntry {
            session,
            ..ev(
                i * 1000,
                SessionEvent::UserInput {
                    text: format!("prompt {i}"),
                },
            )
        });
    }
    // An answer far wider than one 256 KB read step.
    events.push(SessionLogEntry {
        session,
        ..ev(
            5000,
            SessionEvent::AssistantMessage {
                text: "x".repeat(300 * 1024),
                thinking: None,
            },
        )
    });
    for event in events {
        rt.block_on(store.append(event)).expect("append");
    }
    let history = SessionHistory::new(Arc::new(store), session);
    let page = history.turns_before(history.log_size(), 2, PAGE_MAX_BYTES);
    let prompts: Vec<String> = page
        .events
        .iter()
        .filter_map(|e| match &e.entry.event {
            SessionEvent::UserInput { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        prompts,
        vec!["prompt 0", "prompt 1"],
        "the wide event does not hide the turns behind it"
    );
}

/// A cleared session's trajectory is what came after the clear, so a page must
/// not read past it and show turns the session no longer counts.
#[test]
fn test_page_stops_at_clear() {
    let root = temp_dir().join(format!("houyi_history_clear_{}_{}", SessionId::new(), id()));
    create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let rt = Runtime::new().expect("test runtime");
    let session = SessionId::new();
    let events = vec![
        SessionLogEntry {
            session,
            ..ev(
                0,
                SessionEvent::UserInput {
                    text: "before".into(),
                },
            )
        },
        SessionLogEntry {
            session,
            ..ev(10, SessionEvent::ContextCleared { prior_turn: 1 })
        },
        SessionLogEntry {
            session,
            ..ev(
                20,
                SessionEvent::UserInput {
                    text: "after".into(),
                },
            )
        },
    ];
    for event in events {
        rt.block_on(store.append(event)).expect("append");
    }
    let history = SessionHistory::new(Arc::new(store), session);
    let page = history.turns_before(history.log_size(), 10, PAGE_MAX_BYTES);
    let prompts: Vec<String> = page
        .events
        .iter()
        .filter_map(|e| match &e.entry.event {
            SessionEvent::UserInput { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, vec!["after"], "the cleared turn is not shown");
    assert_eq!(
        page.oldest_anchor, None,
        "nothing older than the clear is readable"
    );
}

/// A batch whose complete lines all failed to parse must not stop the walk:
/// the lines were read, so the page moves past them rather than growing its
/// step forever and losing the valid history behind them.
#[test]
fn test_page_skips_corrupt_chunk() {
    let root = temp_dir().join(format!(
        "houyi_history_corrupt_{}_{}",
        SessionId::new(),
        id()
    ));
    create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let rt = Runtime::new().expect("test runtime");
    let session = SessionId::new();
    rt.block_on(store.append(SessionLogEntry {
        session,
        ..ev(0, SessionEvent::UserInput { text: "old".into() })
    }))
    .expect("append old");
    // A batch of complete lines that parse as nothing, wide enough to be a
    // step of its own, sitting between the two turns.
    let log = root.join(session.to_string()).join("log.jsonl");
    if let Some(parent) = log.parent() {
        create_dir_all(parent).ok();
    }
    let mut existing = read_to_string(&log).unwrap_or_default();
    for _ in 0..40 {
        existing.push_str(&format!(
            "{{\"not\":\"an event\",\"pad\":\"{}\"}}\n",
            "x".repeat(8 * 1024)
        ));
    }
    write(&log, existing).expect("write corrupt lines");
    rt.block_on(store.append(SessionLogEntry {
        session,
        ..ev(10, SessionEvent::UserInput { text: "new".into() })
    }))
    .expect("append new");

    let history = SessionHistory::new(Arc::new(store), session);
    let page = history.turns_before(history.log_size(), 5, PAGE_MAX_BYTES);
    let prompts: Vec<String> = page
        .events
        .iter()
        .filter_map(|e| match &e.entry.event {
            SessionEvent::UserInput { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert!(
        prompts.contains(&"old".to_string()),
        "the walk crossed the corrupt batch instead of stopping at it: {prompts:?}"
    );
    assert!(
        page.skipped > 0,
        "and the unreadable lines are reported rather than dropped silently"
    );
}

/// An event wider than the whole page budget must end the walk with a partial
/// page rather than loop or read without bound.
#[test]
fn test_page_caps_oversized_event() {
    let root = temp_dir().join(format!("houyi_history_huge_{}_{}", SessionId::new(), id()));
    create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let rt = Runtime::new().expect("test runtime");
    let session = SessionId::new();
    let events = vec![
        SessionLogEntry {
            session,
            ..ev(0, SessionEvent::UserInput { text: "old".into() })
        },
        // Wider than the whole page budget.
        SessionLogEntry {
            session,
            ..ev(
                10,
                SessionEvent::AssistantMessage {
                    text: "x".repeat((PAGE_MAX_BYTES + 1024) as usize),
                    thinking: None,
                },
            )
        },
    ];
    for event in events {
        rt.block_on(store.append(event)).expect("append");
    }
    let history = SessionHistory::new(Arc::new(store), session);
    let page = history.turns_before(history.log_size(), 5, PAGE_MAX_BYTES);
    assert!(
        page.oldest_partial,
        "a page that could not reach the turns it keeps reports itself partial"
    );
    assert!(
        page.events.len() <= 1,
        "it does not pretend to hold more than it read"
    );
}

/// A budget too small to reach the turn's opening cuts the page and says so,
/// rather than presenting a fragment as a whole turn.
#[test]
fn test_page_budget_reports_partial() {
    let (history, _, _) = session_of(10);
    let page = history.turns_before(history.log_size(), 5, 1);
    assert!(
        page.oldest_partial,
        "the walk ran out of budget before finding the turns it keeps"
    );
}

/// A long session's page is bounded by the page, not by the log: the read
/// walks back only as far as the turns it keeps, so a session that has grown
/// to a hundred thousand events costs the same to open as a short one.
///
/// The log is written directly rather than appended through the store: this
/// measures the read, and driving a hundred thousand appends through the hash
/// chain would measure the writer instead.
#[test]
fn test_long_log_page_bounded() {
    let root = temp_dir().join(format!("houyi_history_long_{}_{}", SessionId::new(), id()));
    create_dir_all(&root).expect("create temp root");
    let session = SessionId::new();
    let log = root.join(session.to_string()).join("log.jsonl");
    create_dir_all(log.parent().expect("log parent")).expect("mkdir session");
    let mut body = String::with_capacity(12 * 1024 * 1024);
    for i in 0..50_000u64 {
        for (offset, event) in [
            SessionEvent::UserInput {
                text: format!("prompt {i}"),
            },
            SessionEvent::AssistantMessage {
                text: format!("answer {i}"),
                thinking: None,
            },
        ]
        .into_iter()
        .enumerate()
        {
            let entry = SessionLogEntry {
                session,
                ..ev(i * 1000 + offset as u64, event)
            };
            body.push_str(&serde_json::to_string(&entry).expect("serialize"));
            body.push('\n');
        }
    }
    write(&log, body).expect("write log");

    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let history = SessionHistory::new(Arc::new(store), session);
    let page = history.turns_before(history.log_size(), 100, PAGE_MAX_BYTES);
    let turns = page
        .events
        .iter()
        .filter(|e| is_user_input(&e.entry))
        .count();
    assert!(
        turns <= 101,
        "the page keeps its turns and one more, not the session: {turns}"
    );
    // The claim is about what was read, not about wall-clock time: a paged
    // read must never read the log whole, and the bytes it asks for must stay
    // inside the disk budget however long the session has grown.
    let (whole_reads, window_reads, requested) = history.read_stats();
    assert_eq!(whole_reads, 0, "a page never reads the log whole");
    assert!(window_reads > 0, "and it does read the bytes it needs");
    assert!(
        requested <= PAGE_READ_MAX_BYTES,
        "the disk budget bounds what a page asks for: {requested}"
    );
    let span = page
        .events
        .last()
        .map(|last| last.byte_offset - page.events[0].byte_offset)
        .unwrap_or(0);
    assert!(
        span < PAGE_MAX_BYTES,
        "and the bytes it holds stay under the page budget: {span}"
    );
}

/// A turn whose opening event is wider than one read step still holds its
/// anchor: a wide prompt must not read as a stale one, which would drop the
/// window back to the tail on the next walk.
#[test]
fn test_anchor_holds_wide_turn() {
    let root = temp_dir().join(format!(
        "houyi_history_anchor_wide_{}_{}",
        SessionId::new(),
        id()
    ));
    create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let rt = Runtime::new().expect("test runtime");
    let session = SessionId::new();
    let mut events = vec![SessionLogEntry {
        session,
        ..ev(
            0,
            SessionEvent::UserInput {
                text: "plain".into(),
            },
        )
    }];
    for (offset, text) in [
        (1000u64, "x".repeat(300 * 1024)),
        (2000, "last".to_string()),
    ] {
        events.push(SessionLogEntry {
            session,
            ..ev(offset, SessionEvent::UserInput { text })
        });
    }
    for event in events {
        rt.block_on(store.append(event)).expect("append");
    }
    let history = SessionHistory::new(Arc::new(store), session);
    let page = history.turns_before(history.log_size(), 2, PAGE_MAX_BYTES);
    let anchor = page
        .oldest_anchor
        .expect("the page starts at the wide turn");
    assert!(
        history.anchor_holds(anchor),
        "the wide turn is the turn the anchor names"
    );
}

/// A clear further back than a few chunks is still found: the head read walks
/// to the event that began the history however far behind it sits, one chunk at
/// a time, and counts every chunk as a read.
/// A cleared history whose clear sits well beyond one scan chunk from the end,
/// with one turn before the clear so the head read has to walk back to find the
/// epoch. Written directly: this measures the read, not the writer.
fn far_clear_history() -> (SessionHistory, EventId) {
    let root = temp_dir().join(format!(
        "houyi_history_far_clear_{}_{}",
        SessionId::new(),
        id()
    ));
    create_dir_all(&root).expect("create temp root");
    let session = SessionId::new();
    let log = root.join(session.to_string()).join("log.jsonl");
    create_dir_all(log.parent().expect("log parent")).expect("mkdir session");
    let mut body = String::with_capacity(20 * 1024 * 1024);
    let before = SessionLogEntry {
        session,
        ..ev(
            0,
            SessionEvent::UserInput {
                text: "cleared away".into(),
            },
        )
    };
    body.push_str(&serde_json::to_string(&before).expect("serialize"));
    body.push('\n');
    let clear = SessionLogEntry {
        session,
        ..ev(1, SessionEvent::ContextCleared { prior_turn: 1 })
    };
    let clear_id = clear.id;
    body.push_str(&serde_json::to_string(&clear).expect("serialize"));
    body.push('\n');
    for i in 0..60_000u64 {
        let entry = SessionLogEntry {
            session,
            ..ev(
                1000 + i,
                SessionEvent::UserInput {
                    text: format!("prompt {i} {}", "x".repeat(200)),
                },
            )
        };
        body.push_str(&serde_json::to_string(&entry).expect("serialize"));
        body.push('\n');
    }
    write(&log, body).expect("write log");
    assert!(
        metadata(&log).expect("stat").len() > 8 * 1024 * 1024,
        "the clear has to sit beyond one scan chunk for this to prove anything"
    );

    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let history = SessionHistory::new(Arc::new(store), session);
    (history, clear_id)
}

#[test]
fn test_head_page_far_clear() {
    let (history, clear_id) = far_clear_history();
    let (_, _, before) = history.read_stats();
    let page = history.head_turns(Some(clear_id), 3, PAGE_MAX_BYTES, &live());
    let prompts: Vec<String> = page
        .events
        .iter()
        .filter_map(|e| match &e.entry.event {
            SessionEvent::UserInput { text } => {
                Some(text.split(' ').nth(1).unwrap_or("").to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        prompts,
        vec!["0", "1", "2"],
        "the head is the first turns after the clear, however far back it sits"
    );
    let (_, _, after) = history.read_stats();
    assert!(
        after - before > PAGE_MAX_BYTES,
        "the walk to the clear is counted as reads: {before} then {after}"
    );
}

/// A head read nobody waits for stops at its next chunk: a superseded walk
/// must not keep reading a long log for a page that will be dropped.
#[test]
fn test_head_page_cancelled() {
    let root = temp_dir().join(format!(
        "houyi_history_head_cancel_{}_{}",
        SessionId::new(),
        id()
    ));
    create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let rt = Runtime::new().expect("test runtime");
    let session = SessionId::new();
    for i in 0..3u64 {
        rt.block_on(store.append(SessionLogEntry {
            session,
            ..ev(
                i * 1000,
                SessionEvent::UserInput {
                    text: format!("prompt {i}"),
                },
            )
        }))
        .expect("append");
    }
    let history = SessionHistory::new(Arc::new(store), session);
    let cancelled = AtomicBool::new(true);
    let page = history.head_turns(None, 3, PAGE_MAX_BYTES, &cancelled);
    assert!(
        page.events.is_empty(),
        "a cancelled head read returns no page rather than reading on"
    );
}

/// A cancelled head read stops at the walk's next check rather than reading the
/// log for a page nobody will take: the epoch sits behind many chunks, so a
/// walk that ignored the flag would read to the beginning of the file and come
/// back with the head instead of nothing. The flag is set before the call, so
/// this proves the check inside the chunk walk exists, not that a flag set by
/// another thread mid-walk is observed at a particular chunk.
#[test]
fn test_head_page_cancel_walk() {
    let (history, clear_id) = far_clear_history();
    let (_, _, before) = history.read_stats();
    let cancelled = AtomicBool::new(true);
    let page = history.head_turns(Some(clear_id), 3, PAGE_MAX_BYTES, &cancelled);
    assert!(
        page.events.is_empty(),
        "a cancelled walk returns no page rather than the head it walked to"
    );
    let (_, _, after) = history.read_stats();
    let size = history.log_size();
    assert!(
        after - before < size / 2,
        "the walk stopped at its next chunk instead of scanning the log: {} of {size}",
        after - before
    );
}

/// A history whose start the epoch scan already discovered is answered from
/// the scan's record: Home reaches it without walking the log again. The walk
/// is the fallback for what the scan has not reached, not the only way to
/// answer.
#[test]
fn test_epoch_start_from_scan() {
    let (history, clear_id) = far_clear_history();
    while !history.scan_chunk().done {}

    let (_, _, before) = history.read_stats();
    let page = history.head_turns(Some(clear_id), 3, PAGE_MAX_BYTES, &live());
    let (_, _, after) = history.read_stats();

    let prompts: Vec<String> = page
        .events
        .iter()
        .filter_map(|e| match &e.entry.event {
            SessionEvent::UserInput { text } => {
                Some(text.split(' ').nth(1).unwrap_or("").to_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        prompts,
        vec!["0", "1", "2"],
        "the head is still the first turns after the clear"
    );
    assert!(
        after - before < PAGE_MAX_BYTES,
        "and the walk did not run for it: {} bytes read",
        after - before
    );
}

/// A history is walked once and then remembered: the first Home pays for the
/// walk, and the next reader of the same history is answered without it.
#[test]
fn test_home_remembers_after_walk() {
    let (history, clear_id) = far_clear_history();
    let (_, _, first_before) = history.read_stats();
    let first = history.head_turns(Some(clear_id), 3, PAGE_MAX_BYTES, &live());
    let (_, _, first_after) = history.read_stats();
    assert!(
        first_after - first_before > PAGE_MAX_BYTES,
        "the first Home walks to the clear: {} bytes",
        first_after - first_before
    );
    assert_eq!(first.events.first().map(|e| e.entry.id), Some(clear_id));

    let (_, _, second_before) = history.read_stats();
    let second = history.head_turns(Some(clear_id), 3, PAGE_MAX_BYTES, &live());
    let (_, _, second_after) = history.read_stats();
    assert!(
        second_after - second_before < PAGE_MAX_BYTES,
        "the second Home is answered from what the walk remembered: {} bytes",
        second_after - second_before
    );
    assert_eq!(second.events.first().map(|e| e.entry.id), Some(clear_id));
}

/// A remembered offset is only trusted when the bytes at it still hold the
/// event that began the history: an offset that names something else is
/// refused and the walk answers instead, rather than starting a page in
/// another history.
#[test]
fn test_home_refuses_stale() {
    let (history, clear_id) = far_clear_history();
    // A start remembered at the wrong byte: the log's first event is not the
    // clear, so the offset must not be trusted.
    history.remember_epoch_start(clear_id, 0);

    let (_, _, before) = history.read_stats();
    let page = history.head_turns(Some(clear_id), 3, PAGE_MAX_BYTES, &live());
    let (_, _, after) = history.read_stats();

    assert_eq!(
        page.events.first().map(|e| e.entry.id),
        Some(clear_id),
        "the head still starts at the clear"
    );
    assert!(
        after - before > PAGE_MAX_BYTES,
        "and the walk ran, because the remembered offset was refused: {} bytes",
        after - before
    );
}

/// Every record a page holds beside its byte offset, in order. The comparison
/// this feeds covers the whole sequence rather than the prompts alone, so an
/// index that dropped a non-prompt event still fails it.
fn page_records(page: &TurnPage) -> Vec<(u64, SessionLogEntry)> {
    page.events
        .iter()
        .map(|e| (e.byte_offset, e.entry.clone()))
        .collect()
}

/// A reader over a store that has seen none of the log: what the next launch
/// finds, which is when a sidecar damaged since it was written is met.
fn reader_over(root: &Path, session: SessionId) -> SessionHistory {
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.to_path_buf())));
    SessionHistory::new(Arc::new(store), session)
}

/// The id of the record the log holds first, the identity a sidecar's header
/// must name for its entries to describe this log's bytes.
fn first_logged_event(log: &Path) -> EventId {
    let mut line = String::new();
    BufReader::new(File::open(log).expect("open the log"))
        .read_line(&mut line)
        .expect("read the first line");
    parse_event(&line).expect("the first line parses").id
}

/// The sidecar index beside a log is derived data, so damage to it must not
/// change what a page read answers. Each damaged file is refused rather than
/// believed, the head page reads the log itself, and the page comes back as it
/// did over the intact file. The log's bytes stay as they were through the
/// whole test, so the index is the only thing that changes and an answer that
/// moved would have moved with it.
///
/// Every case asserts the damage is real first: the store's offset and count
/// queries answer nothing, so a page that still holds its turns is the fallback
/// working rather than a file nobody consulted. The last step restores the file
/// and shows both queries answering, which is what rules out a store that never
/// indexed this log at all.
#[test]
fn test_pages_survive_damage() {
    let (history, session, root) = session_of(12);
    let dir = root.join(session.to_string());
    let index_path = dir.join(INDEX_FILE);
    let intact_head = page_records(&history.head_turns(None, 3, PAGE_MAX_BYTES, &live()));
    let intact_tail = page_records(&history.turns_before(history.log_size(), 3, PAGE_MAX_BYTES));
    // A page keeps whole turns, so its prompt count is the turn count.
    let turns = |records: &[(u64, SessionLogEntry)]| {
        records
            .iter()
            .filter(|(_, entry)| matches!(entry.event, SessionEvent::UserInput { .. }))
            .count()
    };
    assert_eq!(
        turns(&intact_head),
        3,
        "the head page holds the first turns"
    );
    assert_eq!(turns(&intact_tail), 3, "and the tail page the newest ones");
    let recorded = read(&index_path).expect("the appended log carries a sidecar");
    let first = first_logged_event(&dir.join(LOG_FILE));
    let header = LogIndexHeader {
        interval: SAMPLE_INTERVAL_BYTES,
        first_event: Some(first),
        covered_len: history.log_size(),
    };
    let sampled = LogIndexEntry {
        event: first,
        ordinal: 0,
        offset: 0,
    };

    // Four files the store has to refuse: one holding no whole entry, one whose
    // entry a crash left half written, one whose entry records another event,
    // and one whose header claims another log's first record.
    let mut flipped = sampled.encode();
    flipped[0] ^= 0xff;
    let mut torn = header.encode().to_vec();
    torn.extend_from_slice(&sampled.encode()[..(ENTRY_LEN / 2) as usize]);
    let mut misnamed = header.encode().to_vec();
    misnamed.extend_from_slice(&flipped);
    let foreign_header = LogIndexHeader {
        first_event: Some(EventId::new()),
        ..header
    };
    let mut foreign = foreign_header.encode().to_vec();
    foreign.extend_from_slice(&sampled.encode());
    let damages: [(&str, Vec<u8>); 4] = [
        ("a header holding no entry", header.encode().to_vec()),
        ("an entry a crash left half written", torn),
        ("an entry recording another event", misnamed),
        ("an index over another log", foreign),
    ];

    for (damage, bytes) in damages {
        write(&index_path, &bytes).expect("write the damaged sidecar");
        let reader = reader_over(&root, session);
        assert!(
            reader.byte_at(0).is_none(),
            "the store refuses {damage} instead of answering from it"
        );
        assert!(
            reader.event_count().is_none(),
            "and refuses to count the records under {damage}"
        );
        assert_eq!(
            page_records(&reader.head_turns(None, 3, PAGE_MAX_BYTES, &live())),
            intact_head,
            "the head page survives {damage}"
        );
        assert_eq!(
            page_records(&reader.turns_before(reader.log_size(), 3, PAGE_MAX_BYTES)),
            intact_tail,
            "the tail page survives {damage}"
        );
        assert_eq!(
            read(&index_path).expect("the sidecar is still there"),
            bytes,
            "reading pages left {damage} as it was"
        );
    }

    write(&index_path, &recorded).expect("restore the sidecar");
    let reader = reader_over(&root, session);
    assert!(
        reader.byte_at(0).is_some(),
        "the restored sidecar answers the offset query again"
    );
    assert!(
        reader.event_count().is_some(),
        "and counts the log's records again"
    );
    assert_eq!(
        page_records(&reader.head_turns(None, 3, PAGE_MAX_BYTES, &live())),
        intact_head,
        "and the head page comes back as it did before the damage"
    );
}

/// Deterministic pseudo-words for measurement fixtures: derived from the
/// indices so the corpus is reproducible without a random source. Each word
/// is six bytes ("w1234 ").
fn measurement_filler(turn: usize, phase: usize, words: usize) -> String {
    let mut text = String::with_capacity(words * 6 + 16);
    for i in 0..words {
        let v = (turn.wrapping_mul(31) + phase * 17 + i * 29) % 9973;
        text.push_str(&format!("w{v:04} "));
    }
    text
}

/// Running state of the measurement fixture generator: how many bytes have
/// been written, the timestamp for the next event, and how many events have
/// been emitted.
struct MeasurementFixtureState {
    depth_bytes: u64,
    next_ts: u64,
    event_count: usize,
}

/// Serialize one fixture event as a JSONL line, advancing the generator
/// state.
fn write_measurement_event(
    out: &mut BufWriter<File>,
    state: &mut MeasurementFixtureState,
    sid: SessionId,
    event: SessionEvent,
) {
    let entry = SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: state.next_ts,
        prev_hash: None,
        event,
    };
    let mut line = serde_json::to_string(&entry).expect("serialize event");
    line.push('\n');
    state.depth_bytes += line.len() as u64;
    state.next_ts += 1;
    state.event_count += 1;
    out.write_all(line.as_bytes()).expect("write log line");
}

/// One production-shaped turn: a start, a prompt, reasoning, a tool call and
/// its result, and the answer.
fn write_measurement_turn(
    out: &mut BufWriter<File>,
    state: &mut MeasurementFixtureState,
    sid: SessionId,
    turn: usize,
) {
    write_measurement_event(
        out,
        state,
        sid,
        SessionEvent::TurnStarted {
            turn: turn as u32,
            call_in_turn: 0,
        },
    );
    write_measurement_event(
        out,
        state,
        sid,
        SessionEvent::UserInput {
            text: format!("fixture prompt {turn}: inspect the recorded run"),
        },
    );
    write_measurement_event(
        out,
        state,
        sid,
        SessionEvent::Reasoning {
            text: measurement_filler(turn, 1, 300),
        },
    );
    write_measurement_event(
        out,
        state,
        sid,
        SessionEvent::ToolCall {
            call_id: format!("call-{turn}"),
            tool: "bash".into(),
            input: serde_json::json!({ "command": format!("grep -n 'fixture {turn}' run.log") }),
        },
    );
    write_measurement_event(
        out,
        state,
        sid,
        SessionEvent::ToolResult {
            call_id: format!("call-{turn}"),
            output: serde_json::json!({ "stdout": measurement_filler(turn, 2, 500) }),
            duration_ms: (turn % 40) as u64 + 5,
        },
    );
    write_measurement_event(
        out,
        state,
        sid,
        SessionEvent::AssistantMessage {
            text: measurement_filler(turn, 3, 200),
            thinking: None,
        },
    );
}

/// Write a large fixture log directly as JSONL in one buffered pass and
/// return (log_bytes, event_count). The append path fsyncs each line, which
/// a multi-megabyte fixture should not pay; the read side is lenient about
/// the hash chain (prev_hash None), matching the seeding precedent elsewhere.
/// The event shape matches the TUI crate's large-fixture seeder, so the
/// index-build measurement and the page-up measurement compare over the same
/// turn layout; a change here must be mirrored there.
fn seed_measurement_log(root: &Path, sid: SessionId, target_bytes: u64) -> (u64, usize) {
    let dir = root.join(sid.to_string());
    create_dir_all(&dir).expect("mkdir session dir");
    let file = File::create(dir.join(LOG_FILE)).expect("create log");
    let mut out = BufWriter::with_capacity(1 << 20, file);

    let mut state = MeasurementFixtureState {
        depth_bytes: 0,
        next_ts: 1_000,
        event_count: 0,
    };
    for turn in 0..usize::MAX {
        if state.depth_bytes >= target_bytes {
            break;
        }
        write_measurement_turn(&mut out, &mut state, sid, turn);
    }
    out.flush().expect("flush log");
    (state.depth_bytes, state.event_count)
}

/// The current process's resident set in kilobytes, read through the process
/// table tool. Measurement scaffolding, not an engine command.
#[expect(
    clippy::disallowed_methods,
    reason = "measurement sampling of the test process, not an engine command"
)]
fn self_rss_kb() -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &id().to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

/// One scan-and-rebuild run over one fixture size: the timed chunks, the
/// resident-set delta across the build, the sidecar file size, and the
/// finished state for verification.
struct ScanStateRun {
    history: SessionHistory,
    sid: SessionId,
    log_bytes: u64,
    event_count: usize,
    chunk_ms: Vec<f64>,
    build_ms: f64,
    rss_before_kb: Option<u64>,
    rss_after_kb: Option<u64>,
    index_file_bytes: u64,
}

impl ScanStateRun {
    /// Seed a fixture of the target size under root and run the whole scan,
    /// timing every chunk. The fixture is written without a sidecar, so the
    /// first chunk carries the one streaming rebuild that makes offset
    /// queries answer; later chunks are the epoch scan alone.
    fn measure(root: &Path, sid: SessionId, target: u64) -> Self {
        let (log_bytes, event_count) = seed_measurement_log(root, sid, target);
        let store = SessionStore::new(Box::new(LocalFileBackend::new(root.to_path_buf())));
        let history = SessionHistory::new(Arc::new(store), sid);

        let rss_before_kb = self_rss_kb();
        let build_start = Instant::now();
        let mut chunk_ms: Vec<f64> = Vec::new();
        let progress = loop {
            let step = Instant::now();
            let p = history.scan_chunk();
            chunk_ms.push(step.elapsed().as_secs_f64() * 1000.0);
            if p.done {
                break p;
            }
        };
        let build_ms = build_start.elapsed().as_secs_f64() * 1000.0;
        let rss_after_kb = self_rss_kb();
        let index_file_bytes = metadata(root.join(sid.to_string()).join(INDEX_FILE))
            .map(|m| m.len())
            .unwrap_or(0);

        assert_eq!(progress.total_bytes, log_bytes, "scan covered the log");
        assert_eq!(progress.scanned_bytes, log_bytes, "scan reached the head");
        Self {
            history,
            sid,
            log_bytes,
            event_count,
            chunk_ms,
            build_ms,
            rss_before_kb,
            rss_after_kb,
            index_file_bytes,
        }
    }

    /// Correctness rides along with the timing. The count must equal the
    /// fixture's truth; every sidecar entry must answer with its own offset;
    /// positions either side of an entry must land on parseable lines; and
    /// stride positions across the log must stay monotonic and parseable.
    /// An exhaustive sweep is what the resident offset table could afford;
    /// a sidecar query walks from its covering entry, so the net samples the
    /// walk at the boundaries where it restarts instead of paying the log
    /// per position.
    fn verify(&self, root: &Path) {
        let dir = root.join(self.sid.to_string());
        let counted = self.history.event_count().expect("the count answers");
        assert_eq!(counted, self.event_count, "every event counted");

        let file = File::open(dir.join(LOG_FILE)).expect("reopen log");
        let mut first_line = String::new();
        BufReader::new(&file)
            .read_line(&mut first_line)
            .expect("read the first line");
        let first_id = parse_event(&first_line).expect("the first line parses").id;
        let index = LogIndex::load(&dir.join(INDEX_FILE), self.log_bytes, Some(first_id))
            .expect("the sidecar loads against the log");
        assert!(
            index.entries.len() >= 2,
            "the fixture spans several entries: {}",
            index.entries.len()
        );

        for entry in &index.entries {
            let ordinal = usize::try_from(entry.ordinal).expect("ordinal fits");
            assert_eq!(
                self.history.byte_at(ordinal),
                Some(entry.offset),
                "entry {ordinal} answers with its own offset"
            );
            let neighbors = [ordinal + 1, ordinal.saturating_sub(1)];
            for probe in neighbors.into_iter().filter(|p| *p < counted) {
                let off = self
                    .history
                    .byte_at(probe)
                    .unwrap_or_else(|| panic!("position {probe} beside entry {ordinal} answers"));
                assert_parseable(&file, off, probe);
            }
        }
        let mut prev = 0u64;
        for i in (0..counted).step_by(997) {
            let off = self
                .history
                .byte_at(i)
                .unwrap_or_else(|| panic!("stride position {i} answers"));
            assert!(off >= prev, "offsets monotonic at {i}");
            prev = off;
            assert_parseable(&file, off, i);
        }
        // The last record is the one position no entry or stride may skip:
        // an off-by-one at the log's end hides behind every earlier answer.
        let off = self
            .history
            .byte_at(counted - 1)
            .expect("the last position answers");
        assert!(off >= prev, "offsets monotonic at the last position");
        assert_parseable(&file, off, counted - 1);
    }

    fn chunk_stats(&self) -> (f64, f64) {
        let mut sorted = self.chunk_ms.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (sorted[sorted.len() / 2], sorted[sorted.len() - 1])
    }

    fn csv_row(&self) -> String {
        let (p50, max) = self.chunk_stats();
        let rss_text = |r: Option<u64>| r.map(|v| v.to_string()).unwrap_or_else(|| "na".into());
        format!(
            "{},{},{},{:.1},{p50:.1},{max:.1},{},{},{}\n",
            self.log_bytes,
            self.event_count,
            self.chunk_ms.len(),
            self.build_ms,
            rss_text(self.rss_before_kb),
            rss_text(self.rss_after_kb),
            self.index_file_bytes,
        )
    }

    fn report_line(&self) -> String {
        let (p50, max) = self.chunk_stats();
        let rss_text = |r: Option<u64>| r.map(|v| v.to_string()).unwrap_or_else(|| "na".into());
        format!(
            "scan + rebuild {} bytes: {} chunks, {:.1} ms total, p50 {p50:.1} ms, max {max:.1} ms, \
             rss {} -> {} KiB, sidecar {} bytes",
            self.log_bytes,
            self.chunk_ms.len(),
            self.build_ms,
            rss_text(self.rss_before_kb),
            rss_text(self.rss_after_kb),
            self.index_file_bytes,
        )
    }
}

/// The offset at a position must land on a line the log's own parser reads.
fn assert_parseable(file: &File, off: u64, i: usize) {
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(off)).expect("seek");
    let mut line = String::new();
    reader.read_line(&mut line).expect("read line");
    assert!(
        parse_event(&line).is_some(),
        "offset for event {i} lands on a parseable line"
    );
}

/// Baseline for the history scan and sidecar rebuild: chunk times for 16 MiB
/// and 64 MiB logs, the resident-set delta across the build, and the size of
/// the sidecar index file the rebuild leaves beside the log.
#[test]
#[ignore = "measurement workload; run via the history benchmark target"]
fn test_scan_state_baseline() {
    let run_id = var("HOUYI_BENCH_RUN_ID").unwrap_or_else(|_| format!("adhoc-{}", id()));
    let sha = var("HOUYI_GIT_SHA").unwrap_or_else(|_| "unknown".into());
    let run_dir = Path::new("target/measurements/history-index").join(&run_id);
    create_dir_all(&run_dir).expect("measurements dir");

    let sizes = [16u64 * 1024 * 1024, 64 * 1024 * 1024];
    let mut rows = String::from(
        "log_bytes,event_count,chunks,build_ms,chunk_ms_p50,chunk_ms_max,rss_before_kb,rss_after_kb,index_file_bytes\n",
    );

    for target in sizes {
        let root = temp_dir().join(format!("houyi_index_measure_{target}_{}", id()));
        create_dir_all(&root).expect("fixture root");
        let run = ScanStateRun::measure(&root, SessionId::new(), target);
        run.verify(&root);
        rows.push_str(&run.csv_row());
        println!("{}", run.report_line());
        drop(remove_dir_all(&root));
    }

    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    write(
        run_dir.join("meta.txt"),
        format!(
            "sha {sha}\nrun_id {run_id}\nprofile {profile}\nos {} {}\nscan_chunk_bytes \
             {EPOCH_CHUNK_BYTES}\nsizes 16MiB 64MiB\n",
            OS, ARCH
        ),
    )
    .expect("write meta");
    write(run_dir.join("scan_state.csv"), &rows).expect("write rows");
    println!("history index baseline ({})", run_dir.display());
    print!("{rows}");
}
