//! Tests for the shared history reader's typed page reads: a page is a count
//! of turns, the byte budget bounds the walk rather than the page, and a cut
//! turn is reported rather than presented as whole.

use super::*;
use houyicoder_context::{EventId, SessionEvent, SessionLogEntry};

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
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;

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
    let page = history.tail_turns(3, PAGE_MAX_BYTES);
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
        page.older_anchor.is_some(),
        "older turns remain and the anchor points at them"
    );
}

/// Reading older from the page's anchor continues backwards without overlap.
#[test]
fn test_older_page_continues() {
    let (history, _, _) = session_of(10);
    let tail = history.tail_turns(3, PAGE_MAX_BYTES);
    let older = history.turns_before(tail.older_anchor.expect("anchor"), 3, PAGE_MAX_BYTES);
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

/// A log smaller than a page yields everything and reports no older anchor.
#[test]
fn test_page_at_log_start() {
    let (history, _, _) = session_of(2);
    let page = history.tail_turns(5, PAGE_MAX_BYTES);
    assert_eq!(turns_opened(&page.events), 2);
    assert_eq!(page.older_anchor, None, "nothing older to read");
    assert!(!page.oldest_partial);
}

/// A single event wider than one read step must still be found: the reverse
/// read returns nothing until its budget can reach a line's start, so the step
/// has to grow rather than the page giving up.
#[test]
fn test_page_reads_wide_event() {
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;

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
    let page = history.tail_turns(2, PAGE_MAX_BYTES);
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
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;

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
    let page = history.tail_turns(10, PAGE_MAX_BYTES);
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
        page.older_anchor, None,
        "nothing older than the clear is readable"
    );
}

/// A batch whose complete lines all failed to parse must not stop the walk:
/// the lines were read, so the page moves past them rather than growing its
/// step forever and losing the valid history behind them.
#[test]
fn test_page_skips_corrupt_chunk() {
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;

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
    let page = history.tail_turns(5, PAGE_MAX_BYTES);
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
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;

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
    let page = history.tail_turns(5, PAGE_MAX_BYTES);
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
    let page = history.tail_turns(5, 1);
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
    use houyicoder_memory::LocalFileBackend;
    use houyicoder_session::SessionStore;

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
    let page = history.tail_turns(100, PAGE_MAX_BYTES);
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
