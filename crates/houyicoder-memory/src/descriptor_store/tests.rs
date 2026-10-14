//! Tests for this module.

use super::*;
use houyicoder_context::{NameSource, SessionProvenance};

fn sample_descriptor(name: Option<&str>, ts: u64) -> SessionDescriptor {
    SessionDescriptor {
        name: name.map(str::to_string),
        name_source: NameSource::Auto,
        cwd: "/repo".into(),
        model: "test".into(),
        provenance: SessionProvenance::Fresh,
        version: "test".into(),
        created_at: ts,
        child_session_ids: Vec::new(),
    }
}

fn temp_root() -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let p = temp_dir().join(format!("descriptor-test-{}-{n}", id()));
    fs::create_dir_all(&p).expect("mkdir root");
    p
}

#[test]
fn test_descriptor_roundtrip() {
    let root = temp_root();
    let store = FileDescriptorStore::new(root.clone());
    let sid = SessionId::new();
    let descriptor = sample_descriptor(Some("my session"), 100);
    store.write_descriptor(sid, &descriptor).expect("write");
    let back = store.read_descriptor(sid).expect("read");
    assert_eq!(back, descriptor, "descriptor round-trips through disk");
    drop(fs::remove_dir_all(&root));
}

/// The second writer of a session's descriptor loses: the first record is
/// the one that stays, and the second call reports that it wrote nothing.
/// Both stores are checked, since a caller relies on the answer either way.
#[test]
fn test_absent_write_keeps_first() {
    let root = temp_root();
    let sid = SessionId::new();
    let file_store = FileDescriptorStore::new(root.clone());
    assert!(
        file_store
            .write_descriptor_if_absent(sid, &sample_descriptor(Some("first"), 1))
            .expect("first write"),
        "an absent descriptor is written"
    );
    assert!(
        !file_store
            .write_descriptor_if_absent(sid, &sample_descriptor(Some("second"), 2))
            .expect("second write"),
        "a descriptor already there is not written over"
    );

    let memory_store = InMemoryDescriptorStore::new();
    assert!(
        memory_store
            .write_descriptor_if_absent(sid, &sample_descriptor(Some("first"), 1))
            .expect("first write"),
        "an absent descriptor is written"
    );
    assert!(
        !memory_store
            .write_descriptor_if_absent(sid, &sample_descriptor(Some("second"), 2))
            .expect("second write"),
        "a descriptor already there is not written over"
    );
    assert_eq!(
        memory_store.read_descriptor(sid),
        Some(sample_descriptor(Some("first"), 1)),
        "the first record is the one both stores keep"
    );
    assert_eq!(
        file_store.read_descriptor(sid),
        Some(sample_descriptor(Some("first"), 1)),
        "the first record is the one both stores keep"
    );
    drop(fs::remove_dir_all(&root));
}

/// Concurrent edits to one descriptor all survive. Each update appends a
/// char, so the final length counts the edits that landed: a
/// read-modify-write that is not serialized loses whichever edits were
/// derived from a snapshot another writer had already replaced, and the
/// count comes up short. Counting is the point - asserting two named
/// fields both survive passes whenever the interleaving happens to be
/// benign, while a missing char is a lost edit by construction.
#[test]
fn test_concurrent_updates_all_land() {
    const WRITERS: usize = 4;
    const EDITS: usize = 8;
    let root = temp_root();
    let store = FileDescriptorStore::new(root.clone());
    let sid = SessionId::new();
    store
        .write_descriptor(sid, &sample_descriptor(Some(""), 1))
        .expect("seed");
    scope(|scope| {
        for _ in 0..WRITERS {
            scope.spawn(|| {
                for _ in 0..EDITS {
                    store
                        .update_descriptor(sid, &mut |descriptor| {
                            descriptor.name.get_or_insert_with(String::new).push('x');
                        })
                        .expect("update");
                }
            });
        }
    });
    let back = store.read_descriptor(sid).expect("read");
    assert_eq!(
        back.name.as_deref().map(str::len),
        Some(WRITERS * EDITS),
        "every concurrent edit should survive, none overwritten"
    );
    drop(fs::remove_dir_all(&root));
}

/// An update against a session with no descriptor reports Absent rather
/// than creating one. A descriptor materializes on the first durable
/// append; an update is an edit to an existing descriptor, so a rename
/// before that point must not mint a descriptor with default fields.
#[test]
fn test_update_absent_writes_nothing() {
    let root = temp_root();
    let store = FileDescriptorStore::new(root.clone());
    let sid = SessionId::new();
    let mut ran = false;
    let outcome = store
        .update_descriptor(sid, &mut |_| ran = true)
        .expect("update should not error on a missing descriptor");
    assert_eq!(outcome, DescriptorUpdate::Absent, "no descriptor -> Absent");
    assert!(!ran, "the edit closure should not run without a descriptor");
    assert!(
        store.read_descriptor(sid).is_none(),
        "no descriptor was created"
    );
    drop(fs::remove_dir_all(&root));
}

#[test]
fn test_file_store_delete_idempotent() {
    let root = temp_root();
    let store = FileDescriptorStore::new(root.clone());
    let sid = SessionId::new();
    store.delete_descriptor(sid); // missing dir: no panic, no error
    store
        .write_descriptor(sid, &sample_descriptor(None, 1))
        .expect("write");
    store.delete_descriptor(sid);
    assert!(store.read_descriptor(sid).is_none(), "deleted -> absent");
    drop(fs::remove_dir_all(&root));
}

#[test]
fn test_file_store_read_absent() {
    let root = temp_root();
    let store = FileDescriptorStore::new(root.clone());
    let sid = SessionId::new();
    assert!(
        store.read_descriptor(sid).is_none(),
        "no descriptor -> None"
    );
    drop(fs::remove_dir_all(&root));
}

/// The in-memory store honors the same update contract: an edit applies
/// in place and reports Written, a missing session reports Absent. The
/// test tier runs against this impl, so a divergence here would let a
/// caller pass its tests and lose an edit on disk.
#[test]
fn test_in_memory_update_applies() {
    let store = InMemoryDescriptorStore::new();
    let sid = SessionId::new();
    assert_eq!(
        store
            .update_descriptor(sid, &mut |_| ())
            .expect("absent update"),
        DescriptorUpdate::Absent,
        "no entry -> Absent"
    );
    store
        .write_descriptor(sid, &sample_descriptor(Some("before"), 1))
        .expect("write");
    let outcome = store
        .update_descriptor(sid, &mut |descriptor| {
            descriptor.name = Some("after".into())
        })
        .expect("update");
    assert_eq!(
        outcome,
        DescriptorUpdate::Written,
        "entry present -> Written"
    );
    assert_eq!(
        store.read_descriptor(sid).and_then(|m| m.name).as_deref(),
        Some("after"),
        "the edit is visible on the next read"
    );
}

/// A descriptor write resolves the session's directory the same way a read
/// does, so a session whose directory is named in the other spelling is
/// updated in place. Joining the id string unexamined would seal a second
/// descriptor beside no log, and the session would read as unnamed again.
#[test]
fn test_write_resolves_legacy_dir() {
    let root = temp_root();
    let store = FileDescriptorStore::new(root.clone());
    let sid = SessionId::new();
    let legacy = root.join(sid.ulid_name());
    fs::create_dir_all(&legacy).expect("create legacy dir");
    store
        .write_descriptor(sid, &sample_descriptor(Some("first"), 1))
        .expect("seed");
    assert!(
        legacy.join("session.json").is_file(),
        "the descriptor lands in the directory the store holds"
    );

    store
        .write_descriptor(sid, &sample_descriptor(Some("second"), 2))
        .expect("update");
    assert_eq!(
        store.read_descriptor(sid).and_then(|d| d.name).as_deref(),
        Some("second"),
        "the update is visible through the same path"
    );
    assert!(
        !root.join(sid.to_string()).exists(),
        "one session does not get a second directory"
    );
    drop(fs::remove_dir_all(&root));
}

#[test]
fn test_in_memory_round_trips() {
    let store = InMemoryDescriptorStore::new();
    let sid = SessionId::new();
    let descriptor = sample_descriptor(Some("x"), 5);
    store.write_descriptor(sid, &descriptor).expect("write");
    assert_eq!(store.read_descriptor(sid), Some(descriptor));
    store.delete_descriptor(sid);
    assert!(store.read_descriptor(sid).is_none());
}
