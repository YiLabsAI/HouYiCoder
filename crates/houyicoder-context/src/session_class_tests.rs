use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;
use crate::NameSource;

/// Stamp a log's mtime to N seconds ago. Last-active reads the log mtime, so
/// a store built inside one second leaves every session equally recent and no
/// assertion can tell one order from another.
fn age(path: &Path, secs_ago: u64) {
    let t = SystemTime::now() - Duration::from_secs(secs_ago);
    fs::File::options()
        .write(true)
        .open(path)
        .expect("open to age")
        .set_modified(t)
        .expect("set mtime");
}

fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "houyi-session-class-{}-{}-{}",
        tag,
        std::process::id(),
        nanos
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn spawned_by() -> SessionProvenance {
    SessionProvenance::SpawnedBy {
        parent_session_id: "parent".to_string(),
        subagent_type: "explore".to_string(),
        task_id: "task".to_string(),
    }
}

fn sidecar(provenance: SessionProvenance) -> SessionDescriptor {
    SessionDescriptor {
        name: None,
        name_source: NameSource::Auto,
        cwd: "/tmp".to_string(),
        model: "test-model".to_string(),
        provenance,
        version: "0.0.0".to_string(),
        created_at: 1,
        child_session_ids: Vec::new(),
    }
}

fn write_session(
    root: &Path,
    sid: SessionId,
    log: bool,
    provenance: Option<SessionProvenance>,
) -> PathBuf {
    write_session_named(root, &sid.to_string(), log, provenance)
}

fn write_session_named(
    root: &Path,
    name: &str,
    log: bool,
    provenance: Option<SessionProvenance>,
) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    if log {
        std::fs::write(dir.join("log.jsonl"), "{}\n").unwrap();
    }
    if let Some(provenance) = provenance {
        let bytes = serde_json::to_vec(&sidecar(provenance)).unwrap();
        std::fs::write(dir.join("session.json"), bytes).unwrap();
    }
    dir
}

#[test]
fn test_classify_user_needs_sidecar() {
    assert_eq!(classify(true, None), SessionClass::LogOnly);
    assert_eq!(
        classify(true, Some(&SessionProvenance::Fresh)),
        SessionClass::User
    );
    assert_eq!(classify(true, Some(&spawned_by())), SessionClass::SubAgent);
    assert_eq!(
        classify(
            true,
            Some(&SessionProvenance::ResumedFromExport {
                source_session_id: "exported".to_string(),
            })
        ),
        SessionClass::User
    );
}

#[test]
fn test_classify_no_log_shell() {
    assert_eq!(classify(false, None), SessionClass::Shell);
    assert_eq!(
        classify(false, Some(&SessionProvenance::Fresh)),
        SessionClass::Shell
    );
    assert_eq!(classify(false, Some(&spawned_by())), SessionClass::Shell);
}

#[test]
fn test_scan_classifies_every_dir() {
    let root = temp_root("scan");
    let user = write_session(
        &root,
        SessionId::new(),
        true,
        Some(SessionProvenance::Fresh),
    );
    let child = write_session(&root, SessionId::new(), true, Some(spawned_by()));
    let left = write_session(&root, SessionId::new(), true, None);
    let shell = write_session(&root, SessionId::new(), false, None);
    let shell_with_sidecar = write_session(
        &root,
        SessionId::new(),
        false,
        Some(SessionProvenance::Fresh),
    );
    let store = root.join(".cas");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("ab.bin"), b"x").unwrap();
    std::fs::create_dir_all(root.join("index")).unwrap();

    let entries = scan_sessions(&root);
    let class_of = |dir: &Path| entries.iter().find(|e| e.path == dir).map(|e| e.class);
    assert_eq!(entries.len(), 5, "non-session directories must be skipped");
    assert_eq!(class_of(&user), Some(SessionClass::User));
    assert_eq!(class_of(&child), Some(SessionClass::SubAgent));
    assert_eq!(class_of(&left), Some(SessionClass::LogOnly));
    assert_eq!(class_of(&shell), Some(SessionClass::Shell));
    assert_eq!(
        class_of(&shell_with_sidecar),
        Some(SessionClass::Shell),
        "a sidecar with no log is a shell"
    );
    let shell_with_sidecar = entries
        .iter()
        .find(|e| e.path == shell_with_sidecar)
        .unwrap();
    assert!(
        shell_with_sidecar.descriptor.is_none(),
        "a shell's sidecar is not read: the class does not depend on it"
    );
    assert_eq!(entries.iter().filter(|e| e.is_user_session()).count(), 1);
    assert!(entries.iter().all(|e| e.last_active > 0));
    std::fs::remove_dir_all(&root).unwrap();
}

/// A directory named in the legacy spelling is one the id cannot rebuild: the
/// sid prints as a UUID, so a caller holding only the sid looks in a directory
/// that is not there. The entry carries the scanned path and the sidecar for
/// that reason, and a legacy store still yields resumable rows.
#[test]
fn test_entry_carries_scanned_path() {
    const LEGACY: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let root = temp_root("legacy");
    let dir = write_session_named(&root, LEGACY, true, Some(SessionProvenance::Fresh));
    let entries = scan_sessions(&root);
    assert_eq!(entries.len(), 1, "a legacy directory name is a session id");
    let entry = &entries[0];
    assert_eq!(entry.class, SessionClass::User);
    assert_eq!(entry.path, dir);
    assert_ne!(
        entry.path,
        root.join(entry.sid.to_string()),
        "the legacy spelling is not the sid's display form"
    );
    assert!(
        entry.descriptor.is_some(),
        "the sidecar read while classifying is carried"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

/// The id's other spelling is its exact inverse, so a reader can name the
/// directory a pre-uuid store wrote from the id alone.
#[test]
fn test_ulid_name_roundtrips() {
    const LEGACY: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let sid = SessionId::from_display_string(LEGACY).unwrap();
    assert_eq!(sid.ulid_name(), LEGACY, "the spelling round-trips");
    assert_ne!(sid.ulid_name(), sid.to_string());
    let fresh = SessionId::new();
    assert_eq!(
        SessionId::from_display_string(&fresh.ulid_name()),
        Some(fresh),
        "a uuid's ulid spelling parses back to the same id"
    );
}

/// A store opened through the id finds the directory whichever spelling it
/// was written in, and a store with neither gets the display form, which is
/// where a new session is written.
#[test]
fn test_session_dir_resolves_spelling() {
    let root = temp_root("resolve");
    let legacy = SessionId::from_display_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
    let dir = write_session(&root, legacy, true, Some(SessionProvenance::Fresh));
    assert_eq!(
        session_dir(&root, legacy),
        dir,
        "only the ulid spelling exists, so that is the session"
    );

    let both = SessionId::from_display_string("01BX5ZZKBKACTAV9WEVGEMMVRZ").unwrap();
    write_session_named(&root, &both.ulid_name(), true, None);
    let display = write_session(&root, both, true, Some(SessionProvenance::Fresh));
    assert_eq!(
        session_dir(&root, both),
        display,
        "the display form wins when both are on disk"
    );

    let absent = SessionId::new();
    assert_eq!(
        session_dir(&root, absent),
        root.join(absent.to_string()),
        "with nothing on disk the display form is the target for a write"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

/// The bounded listing is the top of the full scan's order rather than a
/// different answer: with the limit at or above the number of user sessions it
/// returns the same ids in the same order, and no non-user directory takes a
/// slot in either. Each session is aged to its own second, so the newest is
/// decided by the store rather than by the tie between three writes.
#[test]
fn test_recent_matches_scan_order() {
    let root = temp_root("recent");
    for secs in [30, 20, 10] {
        let dir = write_session(
            &root,
            SessionId::new(),
            true,
            Some(SessionProvenance::Fresh),
        );
        age(&dir.join(LOG_FILE), secs);
    }
    let child = write_session(&root, SessionId::new(), true, Some(spawned_by()));
    age(&child.join(LOG_FILE), 1);
    write_session(&root, SessionId::new(), true, None);
    write_session(&root, SessionId::new(), false, None);

    let mut full: Vec<SessionEntry> = scan_sessions(&root)
        .into_iter()
        .filter(SessionEntry::is_user_session)
        .collect();
    full.sort_by_key(|entry| std::cmp::Reverse(entry.last_active));

    let bounded = recent_user_sessions(&root, full.len());
    assert_eq!(bounded.len(), 3, "a child, a log-only dir and a shell");
    assert_eq!(
        bounded.iter().map(|e| e.sid).collect::<Vec<_>>(),
        full.iter().map(|e| e.sid).collect::<Vec<_>>(),
        "the bounded walk returns the top of the same order"
    );
    assert_ne!(
        bounded[0].last_active, bounded[2].last_active,
        "the order assertion is about the newest session, not a tie"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

/// The limit bounds the result, and it is applied after the class: a store
/// holding more sessions than the limit returns exactly the limit, and a
/// caller asking for none gets none.
#[test]
fn test_recent_stops_at_limit() {
    let root = temp_root("limit");
    for _ in 0..4 {
        write_session(
            &root,
            SessionId::new(),
            true,
            Some(SessionProvenance::Fresh),
        );
    }
    let got = recent_user_sessions(&root, 2);
    assert_eq!(got.len(), 2, "the limit bounds the result");
    assert!(got.iter().all(SessionEntry::is_user_session));
    assert!(
        recent_user_sessions(&root, 0).is_empty(),
        "a limit of zero is an upper bound like any other"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

/// One session can hold both spellings on disk at once: each directory is a
/// real directory with real files, so the scan reports both. Merging the two
/// is the store's cleanup job, not a reader's, and the resolver's
/// display-first rule is what stops a writer from making a third.
#[test]
fn test_scan_reports_both_spellings() {
    const LEGACY: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let root = temp_root("dupe");
    let sid = SessionId::from_display_string(LEGACY).unwrap();
    write_session_named(&root, LEGACY, true, Some(SessionProvenance::Fresh));
    write_session(&root, sid, true, Some(SessionProvenance::Fresh));

    let entries = scan_sessions(&root);
    assert_eq!(entries.len(), 2, "two directories are two entries");
    assert!(
        entries.iter().all(|entry| entry.sid == sid),
        "both entries are the same session"
    );
    std::fs::remove_dir_all(&root).unwrap();
}
