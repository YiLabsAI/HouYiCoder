//! The resident window: trimming keeps the session's absolute figures, and a
//! paged read reaches the records the window no longer holds.

use super::*;
use crate::trajectory::{DEFAULT_RESIDENT_BYTES, RESIDENT_BUDGET_ENV};
use houyicoder_api::session::TrajectoryPages;
use houyicoder_api::session::TrajectoryPosition;
use serde_json::json;
use std::path::Path;

/// A window budget far smaller than the sessions these tests build, so the
/// window trims within a handful of appends.
const BUDGET_BYTES: usize = 4 * 1024;

/// The text one record carries: wide enough that a few appends outgrow the
/// budget, small enough to stay a plain row.
const TEXT_BYTES: usize = 400;

/// A record wider than the base window a log read asks for, so reaching its
/// line takes a widened window.
const WIDE_BYTES: usize = 2_500_000;

/// A record wider than any window a log read may ask for, so no read returns
/// its line and the readers count it from the bytes they reach instead.
const OVERSIZED_BYTES: usize = 8_500_000;

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = temp_dir().join(format!("log-state-{}-{n}", id()));
    create_dir_all(&path).expect("mkdir root");
    path
}

/// A store over a fresh log root whose window is bounded by BUDGET_BYTES.
fn bounded_store(root: &Path) -> SessionStore {
    SessionStore::with_resident_budget(
        Box::new(LocalFileBackend::new(root.to_path_buf())),
        BUDGET_BYTES,
    )
}

/// Append one record and answer its id, which is what a reader orders by.
async fn append_event(store: &SessionStore, session: SessionId, kind: SessionEvent) -> EventId {
    let id = EventId::new();
    store
        .append(evt(session, id, kind))
        .await
        .expect("the log accepts a record");
    id
}

fn body() -> String {
    "x".repeat(TEXT_BYTES)
}

/// The setting a store is built with is read as the byte count it names, zero
/// included: the window holds its newest row at any budget, so a zero window
/// is one row rather than a window nobody could read.
#[test]
fn test_count_budget_honored() {
    assert_eq!(
        budget_from_setting(Some("65536".into())),
        65_536,
        "a byte count is the budget"
    );
    assert_eq!(
        budget_from_setting(Some("0".into())),
        0,
        "a zero budget is kept, and the window keeps its newest row at it"
    );
}

/// A setting that names no byte count is reported and ignored, so a stray
/// value leaves the compiled budget in place.
#[test]
fn test_unreadable_budget_ignored() {
    assert_eq!(
        budget_from_setting(None),
        DEFAULT_RESIDENT_BYTES,
        "an unset variable leaves the compiled budget"
    );
    assert_eq!(
        budget_from_setting(Some("8mib".into())),
        DEFAULT_RESIDENT_BYTES,
        "a value naming no count leaves the compiled budget"
    );
}

/// The variable name this crate spells is the one the config crate owns, so a
/// rename there cannot leave this reader behind.
#[test]
fn test_budget_env_matches_config() {
    assert_eq!(
        RESIDENT_BUDGET_ENV,
        houyicoder_config::ENV_HOUYICODER_TEST_RESIDENT_BYTE_BUDGET,
        "the session window reads the variable the config crate names"
    );
}

/// A window given no budget still holds its newest row: the trim keeps one row
/// at any budget, so a zero window costs the history behind that row rather
/// than leaving a session with nothing to read.
#[tokio::test]
async fn test_zero_budget_keeps_newest() {
    let root = temp_root();
    let store =
        SessionStore::with_resident_budget(Box::new(LocalFileBackend::new(root.clone())), 0);
    let session = SessionId::new();
    let first = append_event(&store, session, SessionEvent::UserInput { text: body() }).await;
    let second = append_event(&store, session, SessionEvent::UserInput { text: body() }).await;

    let tail = store.trajectory_tail(session);
    assert_eq!(tail.len(), 1, "a zero budget leaves the newest row");
    assert_eq!(tail[0].id, second, "the row it leaves is the newest one");
    assert_ne!(tail[0].id, first, "the row behind it was dropped");
    assert_eq!(
        store.trajectory_head(session).revision.event_count,
        2,
        "the dropped row is still counted"
    );
    remove_dir_all(&root).ok();
}

/// A fold with no budget keeps the newest row too: reading a session back from
/// its log trims by the same rule as an append does, so a session the fold
/// rebuilt still has a row to answer with.
#[tokio::test]
async fn test_zero_budget_survives_fold() {
    let root = temp_root();
    let session = SessionId::new();
    let second;
    {
        let writer = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        append_event(&writer, session, SessionEvent::UserInput { text: body() }).await;
        second = append_event(&writer, session, SessionEvent::UserInput { text: body() }).await;
    }

    let store =
        SessionStore::with_resident_budget(Box::new(LocalFileBackend::new(root.clone())), 0);
    assert_eq!(
        store.restore_trajectory(session).await.unwrap(),
        2,
        "the fold counts every record, held or not"
    );
    let tail = store.trajectory_tail(session);
    assert_eq!(tail.len(), 1, "a zero budget leaves the newest row");
    assert_eq!(tail[0].id, second, "the row it leaves is the newest one");
    remove_dir_all(&root).ok();
}

/// The window drops its oldest rows once it outgrows its budget. Everything a
/// session is said to hold is read from counters, so a trimmed window still
/// answers with the whole session.
#[tokio::test]
async fn test_window_trim_keeps_totals() {
    let root = temp_root();
    let store = bounded_store(&root);
    let session = SessionId::new();
    let mut ids = Vec::new();
    for _ in 0..20 {
        let id = append_event(&store, session, SessionEvent::UserInput { text: body() }).await;
        ids.push(id);
    }
    let last_input = *ids.last().expect("the session holds an input");
    for _ in 0..20 {
        let id = append_event(
            &store,
            session,
            SessionEvent::ToolResult {
                call_id: "call".into(),
                output: json!({ "content": body() }),
                duration_ms: 0,
            },
        )
        .await;
        ids.push(id);
    }

    let tail = store.trajectory_tail(session);
    assert!(
        tail.len() < ids.len(),
        "the window holds fewer rows than the session: {} of {}",
        tail.len(),
        ids.len()
    );
    assert!(
        !tail
            .iter()
            .any(|entry| matches!(entry.event, SessionEvent::UserInput { .. })),
        "the inputs fell below the window, so a reader of the tail alone misses them"
    );
    let revision = store.trajectory_head(session).revision;
    assert_eq!(
        revision.event_count,
        ids.len(),
        "every row is counted, held or not"
    );
    assert_eq!(revision.durable_event_count, ids.len());
    assert_eq!(store.last_trajectory_id(session), ids.last().copied());
    assert_eq!(
        store.last_user_input_id(session),
        Some(last_input),
        "an input the window dropped still answers"
    );
    remove_dir_all(&root).ok();
}

/// The base a reader attaches at: a session with no record names none, a
/// trimmed window names the oldest row it still holds, and a store that has
/// not read the session reaches the same ordinal by folding its log.
#[tokio::test]
async fn test_resident_base_tracks_window() {
    let root = temp_root();
    let store = bounded_store(&root);
    let session = SessionId::new();
    assert_eq!(
        store.resident_base(session).expect("the base reads"),
        None,
        "a session holding no record names no base"
    );

    let rows = (BUDGET_BYTES / TEXT_BYTES) + 4;
    for _ in 0..rows {
        append_event(&store, session, SessionEvent::UserInput { text: body() }).await;
    }
    let base = store
        .resident_base(session)
        .expect("the base reads")
        .expect("the window holds a row");
    assert!(base > 0, "the window trimmed past its own rows: {base}");

    let unread = bounded_store(&root);
    assert_eq!(
        unread.resident_base(session).expect("the base reads"),
        Some(base),
        "folding the log lands on the ordinal the live window reached"
    );
    remove_dir_all(&root).ok();
}

/// A reader that pages sees the records the window no longer holds: the log
/// answers the ordinals below the window and the window answers the rest, once
/// each, in append order.
#[tokio::test]
async fn test_page_walk_covers_history() {
    let root = temp_root();
    let store = bounded_store(&root);
    let session = SessionId::new();
    let mut ids = Vec::new();
    for _ in 0..40 {
        ids.push(append_event(&store, session, SessionEvent::UserInput { text: body() }).await);
    }
    assert!(
        store.trajectory_tail(session).len() < ids.len(),
        "the session has to hold records the window does not, or the walk is one page"
    );

    let mut seen = Vec::new();
    let mut pages = 0;
    for page in TrajectoryPages::new(&store, session, TrajectoryPosition::start()) {
        let page = page.expect("the log answers every page");
        assert_eq!(
            page.skipped_bytes, 0,
            "no record here is wider than a log window"
        );
        seen.extend(page.events.iter().map(|entry| entry.id));
        pages += 1;
    }
    assert_eq!(
        seen, ids,
        "the walk returns every record once, in append order"
    );
    assert!(pages > 1, "the walk crossed from the log into the window");
    remove_dir_all(&root).ok();
}

/// A record wider than the base window a log read asks for is read whole: the
/// read widens until the line is in one window, so the record keeps its place
/// in the history rather than being passed over.
#[tokio::test]
async fn test_wide_record_read_whole() {
    let root = temp_root();
    let session = SessionId::new();
    let first;
    let wide;
    let third;
    {
        let writer = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        first = append_event(
            &writer,
            session,
            SessionEvent::UserInput {
                text: "first".into(),
            },
        )
        .await;
        wide = append_event(
            &writer,
            session,
            SessionEvent::UserInput {
                text: "z".repeat(WIDE_BYTES),
            },
        )
        .await;
        third = append_event(
            &writer,
            session,
            SessionEvent::UserInput {
                text: "third".into(),
            },
        )
        .await;
    }

    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    assert_eq!(
        store.restore_trajectory(session).await.unwrap(),
        3,
        "every record is folded, the wide one included"
    );
    assert_eq!(
        store
            .trajectory_tail(session)
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec![first, wide, third],
        "the fold reads the wide record rather than passing it over"
    );

    let mut seen = Vec::new();
    let mut skipped = 0u64;
    for page in TrajectoryPages::new(&store, session, TrajectoryPosition::start()) {
        let page = page.expect("the log answers every page");
        skipped = skipped.saturating_add(page.skipped_bytes);
        seen.extend(page.events.iter().map(|entry| entry.id));
    }
    assert_eq!(seen, vec![first, wide, third]);
    assert_eq!(skipped, 0, "no record here is wider than a read can hold");
    remove_dir_all(&root).ok();
}

/// A record wider than any window a log read may ask for cannot be read at
/// all. The fold counts its ordinal once from the bytes it reaches and reports
/// them, so the records after it keep their places and a walk does not present
/// the two sides as neighbours.
#[tokio::test]
async fn test_oversized_record_keeps_ordinal() {
    let root = temp_root();
    let session = SessionId::new();
    let first;
    let third;
    {
        let writer = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        first = append_event(
            &writer,
            session,
            SessionEvent::UserInput {
                text: "first".into(),
            },
        )
        .await;
        append_event(
            &writer,
            session,
            SessionEvent::UserInput {
                text: "z".repeat(OVERSIZED_BYTES),
            },
        )
        .await;
        third = append_event(
            &writer,
            session,
            SessionEvent::UserInput {
                text: "third".into(),
            },
        )
        .await;
    }

    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    assert_eq!(
        store.restore_trajectory(session).await.unwrap(),
        3,
        "the record no read holds is still one of the session's ordinals"
    );
    assert_eq!(
        store.trajectory_head(session).revision.durable_event_count,
        3
    );
    assert_eq!(
        store
            .trajectory_tail(session)
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec![third],
        "the fold keeps the records it could read"
    );

    let mut seen = Vec::new();
    let mut skipped = 0u64;
    for page in TrajectoryPages::new(&store, session, TrajectoryPosition::start()) {
        let page = page.expect("the log answers every page");
        skipped = skipped.saturating_add(page.skipped_bytes);
        seen.extend(page.events.iter().map(|entry| entry.id));
    }
    assert_eq!(seen, vec![first, third]);
    assert!(
        skipped > 8_000_000,
        "the walk reports the span it passed over, not a shorter history: {skipped} bytes"
    );
    remove_dir_all(&root).ok();
}

/// An oversized record as the session's first record: the walk reaches the
/// window over a record no read holds, and the page it reports there is empty
/// while its position has moved. The window's own rows are what a reader came
/// for, so the walk has to reach them rather than end on that page.
#[tokio::test]
async fn test_walk_crosses_oversized_first() {
    let root = temp_root();
    let session = SessionId::new();
    let second;
    let third;
    {
        let writer = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        append_event(
            &writer,
            session,
            SessionEvent::UserInput {
                text: "z".repeat(OVERSIZED_BYTES),
            },
        )
        .await;
        second = append_event(
            &writer,
            session,
            SessionEvent::UserInput {
                text: "second".into(),
            },
        )
        .await;
        third = append_event(
            &writer,
            session,
            SessionEvent::UserInput {
                text: "third".into(),
            },
        )
        .await;
    }

    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let mut seen = Vec::new();
    let mut skipped = 0u64;
    for page in TrajectoryPages::new(&store, session, TrajectoryPosition::start()) {
        let page = page.expect("the log answers every page");
        skipped = skipped.saturating_add(page.skipped_bytes);
        seen.extend(page.events.iter().map(|entry| entry.id));
    }
    assert_eq!(
        seen,
        vec![second, third],
        "the walk reaches the window's rows and repeats none of them"
    );
    assert!(
        skipped > 8_000_000,
        "the record no read holds is reported, not dropped: {skipped} bytes"
    );
    remove_dir_all(&root).ok();
}

/// An oversized record that ends the log: the read that reaches its terminator
/// holds no line, and the bytes before it came back as windows of their own.
/// The record's ordinal is taken from the terminator, not from the windows, so
/// the session counts it once.
#[tokio::test]
async fn test_oversized_tail_counts_once() {
    let root = temp_root();
    let session = SessionId::new();
    let first;
    {
        let writer = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
        first = append_event(
            &writer,
            session,
            SessionEvent::UserInput {
                text: "first".into(),
            },
        )
        .await;
        append_event(
            &writer,
            session,
            SessionEvent::UserInput {
                text: "z".repeat(OVERSIZED_BYTES),
            },
        )
        .await;
    }

    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    assert_eq!(
        store.restore_trajectory(session).await.unwrap(),
        2,
        "the record no read holds is counted once, not once per window"
    );
    assert_eq!(
        store.trajectory_head(session).revision.durable_event_count,
        2
    );
    assert_eq!(
        store
            .trajectory_tail(session)
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        vec![first],
        "the fold keeps the records it could read"
    );
    remove_dir_all(&root).ok();
}

/// A walk of a cleared session starts at its epoch: the clear record opens it,
/// so the archived turns before that boundary are not ordinals a reader sees,
/// and the record after it is not read as the first.
#[tokio::test]
async fn test_walk_starts_at_clear() {
    let root = temp_root();
    let store = bounded_store(&root);
    let session = SessionId::new();
    let mut archived = Vec::new();
    for _ in 0..3 {
        archived
            .push(append_event(&store, session, SessionEvent::UserInput { text: body() }).await);
    }
    store.reset_trajectory(session);
    let cleared = append_event(
        &store,
        session,
        SessionEvent::ContextCleared { prior_turn: 1 },
    )
    .await;
    let mut current = vec![cleared];
    for _ in 0..30 {
        current.push(append_event(&store, session, SessionEvent::UserInput { text: body() }).await);
    }
    let tail = store.trajectory_tail(session);
    assert!(
        !tail.iter().any(|entry| entry.id == cleared),
        "the clear record is below the window, so the walk has to read the log for it"
    );

    let mut seen = Vec::new();
    for page in TrajectoryPages::new(&store, session, TrajectoryPosition::start()) {
        let page = page.expect("the log answers every page");
        seen.extend(page.events.iter().map(|entry| entry.id));
    }
    assert_eq!(
        seen, current,
        "the walk returns the epoch from its boundary, and nothing it archived"
    );
    remove_dir_all(&root).ok();
}
