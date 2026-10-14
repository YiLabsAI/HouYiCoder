use super::*;
use houyicoder_context::{
    EventId, NameSource, SessionDescriptor, SessionEvent, SessionId, SessionLogEntry,
    SessionProvenance,
};
use houyicoder_memory::{FileDescriptorStore, LocalFileBackend};
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::fs::FileTimes;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("session-catalog-{}-{n}", id()));
    let _r = remove_dir_all(&p);
    create_dir_all(&p).unwrap();
    p
}

fn descriptor(name: Option<&str>, cwd: &str, ts: u64) -> SessionDescriptor {
    SessionDescriptor {
        name: name.map(str::to_string),
        name_source: NameSource::Auto,
        cwd: cwd.into(),
        model: "test".into(),
        provenance: SessionProvenance::Fresh,
        version: "t".into(),
        created_at: ts,
        child_session_ids: Vec::new(),
    }
}

/// Write a descriptor for a session at the root (real disk, one truth
/// source with the catalog's sessions_root).
fn write_descriptor(root: &Path, sid: SessionId, m: &SessionDescriptor) {
    let store = FileDescriptorStore::new(root.to_path_buf());
    store.write_descriptor(sid, m).unwrap();
}

/// Stamp a path's mtime to N seconds ago so the last-active sort is
/// deterministic. The listing resolves mtime at whole-second
/// granularity, so two sessions written in the same second tie and the
/// sort falls back to readdir order (non-deterministic); ageing each to
/// a distinct second pins the order the tests assert on. The descriptor's
/// created_at field does NOT participate in the sort -- only this mtime
/// does -- so age() is the single ordering signal in these tests.
///
/// Opened for write, not read: on windows the underlying call demands
/// the write-attributes right, which a read-only handle does not carry.
/// Every path passed here is a file, so one open serves both platforms.
fn age(path: &Path, secs_ago: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs_ago);
    let f = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for set_times");
    f.set_times(FileTimes::new().set_modified(t))
        .expect("set mtime");
}

/// Append a UserInput to a session's log so it is resumable + listed by
/// the picker (a session without a log is skipped -- resume_sid
/// hard-errors on a missing log).
async fn append_log(store: &SessionStore, sid: SessionId, text: &str) {
    store
        .append(SessionLogEntry {
            id: EventId::new(),
            session: sid,
            ts: 0,
            prev_hash: None,
            event: SessionEvent::UserInput { text: text.into() },
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn test_catalog_lists_derives_titles() {
    let root = temp_root();
    let cur = SessionId::new();
    let older = SessionId::new();
    let newer = SessionId::new();
    write_descriptor(&root, older, &descriptor(None, "/repo/a", 1));
    write_descriptor(
        &root,
        newer,
        &descriptor(Some("named session"), "/repo/b", 1),
    );
    write_descriptor(&root, cur, &descriptor(None, "/repo/c", 1));
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    append_log(&store, older, "hello world prompt").await;
    append_log(&store, newer, "named session prompt").await;
    // Pin distinct whole-second log mtimes: older newest (sorts first),
    // newer second. age() is the only ordering signal (created_at above
    // is constant and does not sort).
    age(&root.join(older.to_string()).join("log.jsonl"), 100);
    age(&root.join(newer.to_string()).join("log.jsonl"), 200);
    let log: Arc<dyn SessionLog> = Arc::new(store);
    let catalog = DescriptorSessionCatalog::new(log, root.clone());
    let mut rows = catalog.sessions(&cur.to_string());
    assert_eq!(rows.len(), 2, "current session excluded: {rows:?}");
    assert_eq!(
        rows[0].sid_str,
        older.to_string(),
        "newer log mtime sorts first"
    );
    assert!(
        rows[0].title.starts_with("(session) "),
        "sessions returns a placeholder, not the slug: {}",
        rows[0].title
    );
    assert_eq!(
        rows[1].sid_str,
        newer.to_string(),
        "older log mtime sorts second"
    );
    assert_eq!(
        rows[1].title, "named session",
        "descriptor name is the cheap title (no log read)"
    );
    assert_eq!(rows[1].cwd_basename, "b");
    catalog.resolve_detail(&mut rows[0]);
    assert_eq!(
        rows[0].title, "hello-world-prompt",
        "resolve_detail fills the first-prompt slug"
    );
    catalog.resolve_detail(&mut rows[1]);
    assert_eq!(
        rows[1].title, "named session",
        "resolve_detail leaves a descriptor name untouched"
    );
    let _r = remove_dir_all(&root);
}

/// A prompt longer than the slug cap produces an ellipsis-terminated slug
/// no wider than 40 display columns, with no trailing dash before the
/// ellipsis.
#[tokio::test]
async fn test_long_prompt_slug_ellipsis() {
    let root = temp_root();
    let sid = SessionId::new();
    write_descriptor(&root, sid, &descriptor(None, "/repo", 1));
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    append_log(
        &store,
        sid,
        "project context from the nearest agents memory hicoder today",
    )
    .await;
    age(&root.join(sid.to_string()).join("log.jsonl"), 100);
    let log: Arc<dyn SessionLog> = Arc::new(store);
    let catalog = DescriptorSessionCatalog::new(log, root.clone());
    let mut rows = catalog.sessions(&SessionId::new().to_string());
    assert_eq!(rows.len(), 1);
    catalog.resolve_detail(&mut rows[0]);
    let title = &rows[0].title;
    assert!(
        title.ends_with('\u{2026}'),
        "long slug should end with ellipsis: {title}"
    );
    assert!(
        !title.ends_with("-\u{2026}"),
        "no trailing dash before ellipsis: {title}"
    );
    assert!(
        title.chars().count() <= 40,
        "slug within 40 chars: {title} ({})",
        title.chars().count()
    );
    let _r = remove_dir_all(&root);
}

#[tokio::test]
async fn test_catalog_placeholder_no_prompt() {
    let root = temp_root();
    let sid = SessionId::new();
    write_descriptor(&root, sid, &descriptor(None, "/repo", 1));
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    // Append an empty UserInput so the session has a log (resumable +
    // listed) but slugifies to nothing -- the title stays the sid
    // placeholder, the property under test.
    append_log(&store, sid, "").await;
    let log: Arc<dyn SessionLog> = Arc::new(store);
    let catalog = DescriptorSessionCatalog::new(log, root.clone());
    let mut rows = catalog.sessions(&SessionId::new().to_string());
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].title.starts_with("(session) "),
        "placeholder should carry a short sid suffix, got: {}",
        rows[0].title
    );
    assert!(
        rows[0].title.len() > "(session) ".len(),
        "the suffix must add distinguishing info: {}",
        rows[0].title
    );
    catalog.resolve_detail(&mut rows[0]);
    assert!(
        rows[0].title.starts_with("(session) "),
        "placeholder survives resolve_detail when no prompt exists: {}",
        rows[0].title
    );
    let _r = remove_dir_all(&root);
}

#[tokio::test]
async fn test_catalog_dedup_by_title() {
    let root = temp_root();
    let a = SessionId::new();
    let b = SessionId::new();
    let c = SessionId::new();
    write_descriptor(&root, a, &descriptor(Some("shared"), "/repo", 1));
    write_descriptor(&root, b, &descriptor(Some("shared"), "/repo", 1));
    write_descriptor(&root, c, &descriptor(Some("unique"), "/repo", 1));
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    append_log(&store, a, "a prompt").await;
    append_log(&store, b, "b prompt").await;
    append_log(&store, c, "c prompt").await;
    // Pin distinct whole-second log mtimes: a newest (wins the "shared"
    // dedup -- first occurrence kept), c middle (survives, unique
    // title), b oldest (dropped). age() is the only ordering signal.
    age(&root.join(a.to_string()).join("log.jsonl"), 100);
    age(&root.join(b.to_string()).join("log.jsonl"), 300);
    age(&root.join(c.to_string()).join("log.jsonl"), 200);
    let log: Arc<dyn SessionLog> = Arc::new(store);
    let catalog = DescriptorSessionCatalog::new(log, root.clone());
    let rows = catalog.sessions(&SessionId::new().to_string());
    assert_eq!(rows.len(), 2, "dedup drops one of the shared-title pair");
    assert!(
        rows.iter().any(|r| r.sid_str == a.to_string()),
        "newer shared-title session wins the dedup"
    );
    assert!(
        rows.iter().any(|r| r.sid_str == c.to_string()),
        "unique title session survives dedup"
    );
    assert!(
        !rows.iter().any(|r| r.sid_str == b.to_string()),
        "older shared-title session is dropped"
    );
    let _r = remove_dir_all(&root);
}

/// A directory named in the legacy id spelling is reachable: the sid prints
/// as a UUID, so a reader that rebuilds the directory from the id finds
/// nothing and the row disappears. The listing carries the scanned
/// directory and the descriptor, so the row renders either way.
#[tokio::test]
async fn test_catalog_reads_legacy_name() {
    const LEGACY: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let root = temp_root();
    let dir = root.join(LEGACY);
    create_dir_all(&dir).unwrap();
    let sid = SessionId::from_display_string(LEGACY).unwrap();
    assert_ne!(
        dir,
        root.join(sid.to_string()),
        "the fixture is only meaningful while the two spellings differ"
    );
    write(
        dir.join("session.json"),
        serde_json::to_vec(&descriptor(Some("legacy session"), "/repo", 1)).unwrap(),
    )
    .unwrap();
    let entry = SessionLogEntry {
        id: EventId::new(),
        session: sid,
        ts: 0,
        prev_hash: None,
        event: SessionEvent::UserInput {
            text: "legacy prompt".into(),
        },
    };
    write(
        dir.join("log.jsonl"),
        format!("{}\n", serde_json::to_string(&entry).unwrap()),
    )
    .unwrap();

    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    let log: Arc<dyn SessionLog> = Arc::new(store);
    let catalog = DescriptorSessionCatalog::new(log, root.clone());
    let rows = catalog.sessions(&SessionId::new().to_string());
    assert_eq!(rows.len(), 1, "a legacy-named session is still a row");
    assert_eq!(rows[0].sid_str, sid.to_string());
    assert_eq!(rows[0].title, "legacy session");
    assert!(
        rows[0].log_size > 0,
        "the size is read from the directory the scan found"
    );
    let _r = remove_dir_all(&root);
}
