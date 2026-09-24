//! Tests for the shared history reader's typed page reads: a page is a count
//! of turns, the byte budget bounds the walk rather than the page, and a cut
//! turn is reported rather than presented as whole.

use super::*;
use houyicoder_context::{EventId, SessionEvent, SessionLogEntry};
use houyicoder_memory::LocalFileBackend;
use houyicoder_session::SessionStore;

/// A cancel flag no read sets: a test drives the read to its end.
fn live() -> std::sync::atomic::AtomicBool {
    std::sync::atomic::AtomicBool::new(false)
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
fn session_of(turns: usize) -> (SessionHistory, SessionId, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "houyi_history_page_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
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
    let history = SessionHistory::new(std::sync::Arc::new(store), session);
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
    let root = std::env::temp_dir().join(format!(
        "houyi_history_head_clear_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
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

    let history = SessionHistory::new(std::sync::Arc::new(store), session);
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
    let root = std::env::temp_dir().join(format!(
        "houyi_history_head_wide_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
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
    let history = SessionHistory::new(std::sync::Arc::new(store), session);
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
    let root = std::env::temp_dir().join(format!(
        "houyi_history_wide_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
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
    let history = SessionHistory::new(std::sync::Arc::new(store), session);
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
    let root = std::env::temp_dir().join(format!(
        "houyi_history_clear_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
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
    let history = SessionHistory::new(std::sync::Arc::new(store), session);
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
    let root = std::env::temp_dir().join(format!(
        "houyi_history_corrupt_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
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
        std::fs::create_dir_all(parent).ok();
    }
    let mut existing = std::fs::read_to_string(&log).unwrap_or_default();
    for _ in 0..40 {
        existing.push_str(&format!(
            "{{\"not\":\"an event\",\"pad\":\"{}\"}}\n",
            "x".repeat(8 * 1024)
        ));
    }
    std::fs::write(&log, existing).expect("write corrupt lines");
    rt.block_on(store.append(SessionLogEntry {
        session,
        ..ev(10, SessionEvent::UserInput { text: "new".into() })
    }))
    .expect("append new");

    let history = SessionHistory::new(std::sync::Arc::new(store), session);
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
    let root = std::env::temp_dir().join(format!(
        "houyi_history_huge_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
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
    let history = SessionHistory::new(std::sync::Arc::new(store), session);
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
    let root = std::env::temp_dir().join(format!(
        "houyi_history_long_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let session = SessionId::new();
    let log = root.join(session.to_string()).join("log.jsonl");
    std::fs::create_dir_all(log.parent().expect("log parent")).expect("mkdir session");
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
    std::fs::write(&log, body).expect("write log");

    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let history = SessionHistory::new(std::sync::Arc::new(store), session);
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
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;

    let root = std::env::temp_dir().join(format!(
        "houyi_history_anchor_wide_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
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
    let history = SessionHistory::new(std::sync::Arc::new(store), session);
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
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;

    let root = std::env::temp_dir().join(format!(
        "houyi_history_far_clear_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let session = SessionId::new();
    let log = root.join(session.to_string()).join("log.jsonl");
    std::fs::create_dir_all(log.parent().expect("log parent")).expect("mkdir session");
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
    std::fs::write(&log, body).expect("write log");
    assert!(
        std::fs::metadata(&log).expect("stat").len() > 8 * 1024 * 1024,
        "the clear has to sit beyond one scan chunk for this to prove anything"
    );

    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let history = SessionHistory::new(std::sync::Arc::new(store), session);
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
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;

    let root = std::env::temp_dir().join(format!(
        "houyi_history_head_cancel_{}_{}",
        SessionId::new(),
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("create temp root");
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root)));
    let rt = tokio::runtime::Runtime::new().expect("test runtime");
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
    let history = SessionHistory::new(std::sync::Arc::new(store), session);
    let cancelled = std::sync::atomic::AtomicBool::new(true);
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
    let cancelled = std::sync::atomic::AtomicBool::new(true);
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
