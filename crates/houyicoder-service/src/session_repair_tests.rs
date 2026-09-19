//! Peer tests for the lineage repair pass: a child whose boundary write was
//! lost gets the descriptor its log implies, a record the store already
//! holds is kept, and a store that cannot write is counted without stopping
//! the pass. The pass's effect on what a sweep removes is pinned with the
//! plan it reads from.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use houyicoder_context::session_class::{
    DESCRIPTOR_FILE, LOG_FILE, ParentLink, SessionClass, scan_sessions,
};
use houyicoder_context::{DescriptorUpdate, EventId, NameSource, SessionEvent, SessionLogEntry};
use houyicoder_memory::FileDescriptorStore;

fn temp_root() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("houyi-repair-{seq}-{}", std::process::id()));
    let _r = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).expect("mkdir root");
    d
}

/// A session directory holding the record production writes as a log line.
/// The timestamp is in the unit the log records, milliseconds.
fn write_log(dir: &Path, event: SessionEvent, ts_ms: u64) {
    fs::create_dir_all(dir).expect("mkdir session");
    let entry = SessionLogEntry {
        id: EventId::new(),
        session: SessionId::new(),
        ts: ts_ms,
        prev_hash: None,
        event,
    };
    let line = serde_json::to_string(&entry).expect("serialize record");
    fs::write(dir.join(LOG_FILE), format!("{line}\n")).expect("log");
}

/// A child whose descriptor never landed: the log opens with the delegation its
/// boundary writes. Returns the directory and the id its name spells.
fn child_log_only(root: &Path, parent: &str, ts_ms: u64) -> (PathBuf, SessionId) {
    let sid = SessionId::new();
    let dir = root.join(sid.to_string());
    write_log(
        &dir,
        SessionEvent::ChildDelegated {
            parent_session_id: parent.to_string(),
            subagent_type: "explore".to_string(),
        },
        ts_ms,
    );
    (dir, sid)
}

fn descriptor(provenance: SessionProvenance) -> SessionDescriptor {
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

/// A store that refuses every write, so the pass's failure path is exercised
/// without manufacturing a filesystem permission error.
struct RefusingStore;

impl SessionDescriptorStore for RefusingStore {
    fn read_descriptor(&self, _session: SessionId) -> Option<SessionDescriptor> {
        None
    }

    fn write_descriptor(
        &self,
        _session: SessionId,
        _descriptor: &SessionDescriptor,
    ) -> Result<(), SessionDescriptorError> {
        Err(SessionDescriptorError("refused".to_string()))
    }

    fn update_descriptor(
        &self,
        _session: SessionId,
        _edit: &mut dyn FnMut(&mut SessionDescriptor),
    ) -> Result<DescriptorUpdate, SessionDescriptorError> {
        Err(SessionDescriptorError("refused".to_string()))
    }

    fn write_descriptor_if_absent(
        &self,
        _session: SessionId,
        _descriptor: &SessionDescriptor,
    ) -> Result<bool, SessionDescriptorError> {
        Err(SessionDescriptorError("refused".to_string()))
    }

    fn delete_descriptor(&self, _session: SessionId) {}
}

/// The child's descriptor is written from its own log: the parent the log
/// names, the agent type it ran as, and the second that record carries,
/// which is when the child was created.
#[test]
fn test_repair_writes_child_descriptor() {
    let root = temp_root();
    let (_dir, sid) = child_log_only(&root, "parent-1", 1_700_000_042_123);
    let store = FileDescriptorStore::new(root.clone());
    let repair = repair_child_descriptors_in(&root, &store);
    assert_eq!(
        repair.written, 1,
        "the child gets the descriptor its log implies"
    );
    assert_eq!(repair.failed, 0, "nothing failed");
    let descriptor = store.read_descriptor(sid).expect("a descriptor landed");
    assert_eq!(
        descriptor.provenance,
        SessionProvenance::SpawnedBy {
            parent_session_id: "parent-1".to_string(),
            subagent_type: "explore".to_string(),
            task_id: sid.to_string(),
        },
        "the descriptor names the delegation the log opens with"
    );
    assert_eq!(
        descriptor.created_at, 1_700_000_042,
        "created at the second the delegation was recorded, not its millisecond"
    );
    assert!(descriptor.cwd.is_empty() && descriptor.model.is_empty());
    let entry = scan_sessions(&root)
        .into_iter()
        .find(|e| e.sid == sid)
        .expect("the session is scanned");
    assert_eq!(
        entry.class,
        SessionClass::SubAgent,
        "the class is unchanged"
    );
    assert!(
        entry.parent.is_none(),
        "the descriptor answers now, so the log is not opened"
    );
    let _r = fs::remove_dir_all(&root);
}

/// A descriptor the store holds is the record the boundary wrote: the pass
/// leaves its fields alone and reports the child as one it did not write.
/// The record here is deliberately not the one the pass would write, so
/// keeping it is what the test can fail on.
#[test]
fn test_repair_keeps_existing_descriptor() {
    let root = temp_root();
    let (_dir, sid) = child_log_only(&root, "parent-1", 1_700_000_042_123);
    let mut kept = descriptor(SessionProvenance::Fresh);
    kept.name = Some("kept".to_string());
    kept.cwd = "/repo".to_string();
    let store = FileDescriptorStore::new(root.clone());
    store.write_descriptor(sid, &kept).expect("seed descriptor");

    let repair = repair_child_descriptors_in(&root, &store);
    assert_eq!(
        repair.written, 0,
        "a descriptor that is there is not written"
    );
    assert_eq!(
        store.read_descriptor(sid).expect("descriptor kept"),
        kept,
        "the record the boundary wrote keeps its fields"
    );
    let _r = fs::remove_dir_all(&root);
}

/// The store decides whether a child needs a descriptor, not the scan: a
/// record landed between the two is the one that stays, and the pass reports
/// no write for it.
#[test]
fn test_repair_defers_to_store() {
    let root = temp_root();
    let (_dir, sid) = child_log_only(&root, "parent-1", 1_700_000_042_123);
    let link = ParentLink {
        parent_session_id: "parent-1".to_string(),
        subagent_type: "explore".to_string(),
        created_at_secs: 1_700_000_042,
    };
    let store = FileDescriptorStore::new(root.clone());
    let landed = descriptor(SessionProvenance::SpawnedBy {
        parent_session_id: "parent-1".to_string(),
        subagent_type: "explore".to_string(),
        task_id: "spawn-task".to_string(),
    });
    store
        .write_descriptor(sid, &landed)
        .expect("seed descriptor");

    let wrote = write_child_descriptor_if_absent(&store, sid, &link).expect("the store answers");
    assert!(!wrote, "the record that landed first is the one kept");
    assert_eq!(
        store.read_descriptor(sid).expect("descriptor kept"),
        landed,
        "the pass writes no second record over it"
    );
    let _r = fs::remove_dir_all(&root);
}

/// Only a delegation produces a write: a log whose first record is not one,
/// a directory with no log, and a session that is the user's own all keep no
/// descriptor from this pass.
#[test]
fn test_repair_touches_only_delegated() {
    let root = temp_root();
    let user = root.join(SessionId::new().to_string());
    write_log(
        &user,
        SessionEvent::UserInput {
            text: "hello".to_string(),
        },
        1_700_000_042_123,
    );
    let store = FileDescriptorStore::new(root.clone());
    store
        .write_descriptor(
            SessionId::from_display_string(
                user.file_name().and_then(|n| n.to_str()).expect("name"),
            )
            .expect("sid parses"),
            &descriptor(SessionProvenance::Fresh),
        )
        .expect("seed descriptor");
    let shell = root.join(SessionId::new().to_string());
    fs::create_dir_all(&shell).expect("mkdir shell");

    let repair = repair_child_descriptors_in(&root, &store);
    assert_eq!(repair.written, 0, "nothing here is a delegation");
    assert_eq!(repair.failed, 0);
    assert!(
        user.join(DESCRIPTOR_FILE).is_file(),
        "the user session keeps the descriptor it had"
    );
    assert!(
        !shell.join(DESCRIPTOR_FILE).exists(),
        "a directory with no log gets none"
    );
    let _r = fs::remove_dir_all(&root);
}

/// A store that refuses writes is counted, and the pass goes on: two
/// children both reach it, so one failure cannot hide the rest.
#[test]
fn test_repair_counts_failure() {
    let root = temp_root();
    child_log_only(&root, "parent-1", 1_700_000_042_123);
    child_log_only(&root, "parent-2", 1_700_000_043_456);
    let repair = repair_child_descriptors_in(&root, &RefusingStore);
    assert_eq!(repair.written, 0, "no write lands");
    assert_eq!(repair.failed, 2, "both children were attempted");
    let _r = fs::remove_dir_all(&root);
}

/// A child already recorded by its descriptor is not written twice, and a scan
/// run after the pass finds every child still a child: the pass moves where
/// the lineage is read from, not what the store holds.
#[test]
fn test_repair_is_idempotent() {
    let root = temp_root();
    child_log_only(&root, "parent-1", 1_700_000_042_123);
    let store = FileDescriptorStore::new(root.clone());
    assert_eq!(repair_child_descriptors_in(&root, &store).written, 1);
    let second = repair_child_descriptors_in(&root, &store);
    assert_eq!(second.written, 0, "the pass has nothing left to do");
    let children = scan_sessions(&root)
        .into_iter()
        .filter(|e| e.class == SessionClass::SubAgent)
        .count();
    assert_eq!(children, 1, "the child is still a child");
    let _r = fs::remove_dir_all(&root);
}

/// The entry the sweep and the manual cleanup call builds the store at the
/// root it was handed, so the descriptor lands in the child's own directory
/// beside the log it was read from, and no second directory is made there.
#[test]
fn test_repair_entry_roots() {
    let root = temp_root();
    let (dir, _sid) = child_log_only(&root, "parent-1", 1_700_000_042_123);
    let repair = repair_child_descriptors(&root);
    assert_eq!(repair.written, 1, "the child's lost descriptor is written");
    assert_eq!(repair.failed, 0);
    assert!(
        dir.join(DESCRIPTOR_FILE).is_file(),
        "the descriptor lands with the log it came from"
    );
    let others = fs::read_dir(&root)
        .expect("read root")
        .flatten()
        .filter(|e| e.path() != dir)
        .count();
    assert_eq!(others, 0, "the write makes no second directory here");
    let _r = fs::remove_dir_all(&root);
}
