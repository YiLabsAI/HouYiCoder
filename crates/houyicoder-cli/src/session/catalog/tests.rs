use super::*;
use houyicoder_context::session_class::session_page;
use houyicoder_context::{
    EventId, NameSource, PromptCoverage, PromptRecord, SessionDescriptor, SessionDescriptorStore,
    SessionEvent, SessionId, SessionLogEntry, SessionProvenance,
};
use houyicoder_memory::{FileDescriptorStore, LocalFileBackend};
use houyicoder_session::SessionStore;
use std::env::temp_dir;
use std::env::var;
use std::fs::FileTimes;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("session-catalog-{}-{n}", id()));
    let _r = remove_dir_all(&p);
    create_dir_all(&p).unwrap();
    p
}

fn base(cwd: &str, ts: u64) -> SessionDescriptor {
    SessionDescriptor {
        name: None,
        name_source: NameSource::Auto,
        cwd: cwd.into(),
        model: "test".into(),
        provenance: SessionProvenance::Fresh,
        version: "t".into(),
        created_at: ts,
        child_session_ids: Vec::new(),
        first_prompt: None,
        last_prompt: None,
        prompt_coverage: PromptCoverage::Complete,
    }
}

fn named(mut descriptor: SessionDescriptor, name: &str, source: NameSource) -> SessionDescriptor {
    descriptor.name = Some(name.into());
    descriptor.name_source = source;
    descriptor
}

fn says(mut descriptor: SessionDescriptor, first: &str, last: &str) -> SessionDescriptor {
    let record = |text: &str| PromptRecord {
        text: text.into(),
        event_id: EventId::new(),
    };
    descriptor.first_prompt = Some(record(first));
    descriptor.last_prompt = Some(record(last));
    descriptor
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

/// The title column takes the newest prompt the session states, and a name
/// the user gave outranks both prompts. Rows arrive newest-first.
#[tokio::test]
async fn test_catalog_titles_from_record() {
    let root = temp_root();
    let cur = SessionId::new();
    let older = SessionId::new();
    let newer = SessionId::new();
    write_descriptor(
        &root,
        older,
        &says(
            base("/repo/a", 1),
            "hello world prompt",
            "fix the cache table",
        ),
    );
    write_descriptor(
        &root,
        newer,
        &named(
            says(
                base("/repo/b", 1),
                "hello world prompt",
                "fix the cache table",
            ),
            "named session",
            NameSource::User,
        ),
    );
    write_descriptor(&root, cur, &base("/repo/c", 1));
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    append_log(&store, older, "hello world prompt").await;
    append_log(&store, newer, "named session prompt").await;
    // Pin distinct whole-second log mtimes: older newest (sorts first),
    // newer second. age() is the only ordering signal (created_at above
    // is constant and does not sort).
    age(&root.join(older.to_string()).join("log.jsonl"), 100);
    age(&root.join(newer.to_string()).join("log.jsonl"), 200);
    let catalog = DescriptorSessionCatalog::new(root.clone());
    let rows = catalog.sessions(&cur.to_string());
    assert_eq!(rows.len(), 2, "current session excluded: {rows:?}");
    assert_eq!(
        rows[0].sid_str,
        older.to_string(),
        "newer log mtime sorts first"
    );
    assert_eq!(
        rows[0].title, "fix the cache table",
        "the newest prompt names the work, without a log read"
    );
    assert_eq!(rows[0].cwd_basename, "a");
    assert_eq!(
        rows[1].sid_str,
        newer.to_string(),
        "older log mtime sorts second"
    );
    assert_eq!(
        rows[1].title, "named session",
        "a name the user chose outranks the prompts"
    );
    let _r = remove_dir_all(&root);
}

/// A prompt keeps its own spelling in the title column: spaces, capitals and
/// punctuation are what the user wrote, and the column trims by display
/// width at render time rather than rewriting the text.
#[tokio::test]
async fn test_catalog_keeps_prompt_text() {
    const PROMPT: &str = "Project Context from the Nearest AGENTS Memory, today";
    let root = temp_root();
    let sid = SessionId::new();
    write_descriptor(&root, sid, &says(base("/repo", 1), PROMPT, PROMPT));
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    append_log(&store, sid, PROMPT).await;
    age(&root.join(sid.to_string()).join("log.jsonl"), 100);
    let catalog = DescriptorSessionCatalog::new(root.clone());
    let rows = catalog.sessions(&SessionId::new().to_string());
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].title, PROMPT,
        "the prompt text is carried as written"
    );
    let _r = remove_dir_all(&root);
}

#[tokio::test]
async fn test_catalog_placeholder_without_prompt() {
    let root = temp_root();
    let sid = SessionId::new();
    write_descriptor(&root, sid, &base("/repo", 1));
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    // Append an empty UserInput so the session has a log (resumable +
    // listed) but states no task -- the title stays the sid placeholder,
    // the property under test.
    append_log(&store, sid, "").await;
    let catalog = DescriptorSessionCatalog::new(root.clone());
    let rows = catalog.sessions(&SessionId::new().to_string());
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
    let _r = remove_dir_all(&root);
}

/// Two sessions named alike are two sessions: the listing keeps both so the
/// user can tell them apart by time and project instead of losing one.
#[tokio::test]
async fn test_catalog_keeps_duplicate_rows() {
    let root = temp_root();
    let a = SessionId::new();
    let b = SessionId::new();
    let c = SessionId::new();
    let shared = |cwd: &str| named(base(cwd, 1), "shared", NameSource::User);
    write_descriptor(&root, a, &shared("/repo/one"));
    write_descriptor(&root, b, &shared("/repo/two"));
    write_descriptor(
        &root,
        c,
        &named(base("/repo", 1), "unique", NameSource::User),
    );
    let store = SessionStore::new(Box::new(LocalFileBackend::new(root.clone())));
    append_log(&store, a, "a prompt").await;
    append_log(&store, b, "b prompt").await;
    append_log(&store, c, "c prompt").await;
    age(&root.join(a.to_string()).join("log.jsonl"), 100);
    age(&root.join(b.to_string()).join("log.jsonl"), 300);
    age(&root.join(c.to_string()).join("log.jsonl"), 200);
    let catalog = DescriptorSessionCatalog::new(root.clone());
    let rows = catalog.sessions(&SessionId::new().to_string());
    assert_eq!(rows.len(), 3, "no row hides behind a matching title");
    assert_eq!(rows[0].sid_str, a.to_string(), "newest first");
    assert_eq!(rows[1].sid_str, c.to_string(), "middle by mtime");
    assert_eq!(rows[2].sid_str, b.to_string(), "oldest last");
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
        serde_json::to_vec(&named(base("/repo", 1), "legacy session", NameSource::User)).unwrap(),
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

    let catalog = DescriptorSessionCatalog::new(root.clone());
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

/// A project directory names the row by its last segment, whether or not the
/// record ends in a separator; a path with no segment reads as unknown.
#[test]
fn test_basename_takes_last_segment() {
    assert_eq!(cwd_basename("/work/repo"), "repo");
    assert_eq!(cwd_basename("/work/repo/"), "repo");
    assert_eq!(cwd_basename("/"), "?");
}

/// What the picker's first frame costs over ten thousand sessions, and what
/// one backfill page costs over the same store. A measurement rather than a
/// gate: it reports its numbers and asserts only the row bounds, and it stays
/// out of the ignored suites unless asked for one by name.
#[test]
#[ignore = "measurement"]
fn test_listing_scale_cost() {
    if var("HOUYICODER_MEASURE_SCALE").as_deref() != Ok("1") {
        return;
    }
    let root = temp_root();
    for i in 0..10_000u64 {
        let sid = SessionId::new();
        let dir = root.join(sid.to_string());
        create_dir_all(&dir).unwrap();
        let record = says(
            base("/work/repo", 1_000 + i % 100),
            "fix the login test",
            "ship it",
        );
        write(
            dir.join("session.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        write(dir.join("log.jsonl"), "").unwrap();
    }
    let catalog = DescriptorSessionCatalog::new(root.clone());
    let started = Instant::now();
    let rows = catalog.sessions(&SessionId::new().to_string());
    let listing = started.elapsed();
    assert_eq!(rows.len(), 100, "the listing stops at its visible limit");
    let started = Instant::now();
    let page = session_page(&root, None, 400);
    let paging = started.elapsed();
    assert_eq!(page.len(), 400, "the page stops at its bound");
    println!("ten thousand sessions: picker listing {listing:?}, backfill page {paging:?}");
    let _r = remove_dir_all(&root);
}
