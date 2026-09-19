//! Disk and in-memory stores for session descriptors. The disk store writes
//! <root>/<sid>/session.json atomically beside the event log.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use houyicoder_context::{
    DescriptorUpdate, SessionDescriptor, SessionDescriptorError, SessionDescriptorStore, SessionId,
    SessionProvenance,
    session_class::{DESCRIPTOR_FILE, session_dir},
};

/// Disk-backed descriptor store rooted beside the session event logs.
pub struct FileDescriptorStore {
    root: PathBuf,
    /// One lock per session, guarding a whole write or read-modify-write so
    /// two callers touching the same descriptor serialize instead of each
    /// publishing a copy derived from the state it read. Per session, not
    /// one lock for the store: the write ends in an fsync, and one session's
    /// slow flush must not stall an unrelated session's write. Entries are
    /// never evicted - removing one while another thread holds it would hand
    /// the next caller a fresh lock and silently drop the exclusion - so the
    /// map grows with the number of distinct sessions touched in a process,
    /// which is the session count, not a leak that tracks time.
    locks: Mutex<HashMap<SessionId, Arc<Mutex<()>>>>,
}

impl FileDescriptorStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            locks: Mutex::new(HashMap::new()),
        }
    }

    /// The lock for a session, created on first use. The map guard is
    /// released before the caller takes the session lock, so a slow fsync
    /// under one session never blocks another session from looking its own
    /// lock up.
    fn session_lock(&self, session: SessionId) -> Arc<Mutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .expect("descriptor lock map poisoned")
                .entry(session)
                .or_default(),
        )
    }

    fn descriptor_path(&self, session: SessionId) -> PathBuf {
        session_dir(&self.root, session).join(DESCRIPTOR_FILE)
    }

    fn ensure_dir(path: &Path) -> Result<(), SessionDescriptorError> {
        #[cfg(unix)]
        {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|e| SessionDescriptorError(format!("mkdir {path:?}: {e}")))
        }
        #[cfg(not(unix))]
        {
            fs::create_dir_all(path)
                .map_err(|e| SessionDescriptorError(format!("mkdir {path:?}: {e}")))
        }
    }

    fn write_sync(
        &self,
        session: SessionId,
        descriptor: &SessionDescriptor,
    ) -> Result<(), SessionDescriptorError> {
        let dir = session_dir(&self.root, session);
        Self::ensure_dir(&dir)?;
        let path = self.descriptor_path(session);
        let body = serde_json::to_vec_pretty(descriptor)
            .map_err(|e| SessionDescriptorError(format!("serialize descriptor: {e}")))?;
        // A sibling rename prevents partial descriptors. The pid and counter
        // keep concurrent writers from sharing a temporary file.
        static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
        let tmp = dir.join(format!(
            "session.json.tmp.{}.{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        {
            #[cfg(unix)]
            let mut opts = {
                let mut o = fs::OpenOptions::new();
                o.write(true).create(true).truncate(true).mode(0o600);
                o
            };
            #[cfg(not(unix))]
            let mut opts = fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            let mut f = opts
                .open(&tmp)
                .map_err(|e| SessionDescriptorError(format!("open tmp {tmp:?}: {e}")))?;
            f.write_all(&body)
                .map_err(|e| SessionDescriptorError(format!("write tmp {tmp:?}: {e}")))?;
            f.sync_all()
                .map_err(|e| SessionDescriptorError(format!("sync tmp {tmp:?}: {e}")))?;
        }
        fs::rename(&tmp, &path)
            .map_err(|e| SessionDescriptorError(format!("rename {tmp:?} -> {path:?}: {e}")))
    }

    fn read_sync(&self, session: SessionId) -> Option<SessionDescriptor> {
        let path = self.descriptor_path(session);
        let body = fs::read_to_string(&path).ok()?;
        // A truncated/corrupt descriptor is tolerated as absent rather than
        // fatal: the resume path falls back to deriving cwd/model from the
        // current config, which is safer than refusing to start.
        serde_json::from_str(&body).ok()
    }
}

impl SessionDescriptorStore for FileDescriptorStore {
    fn read_descriptor(&self, session: SessionId) -> Option<SessionDescriptor> {
        self.read_sync(session)
    }

    fn write_descriptor(
        &self,
        session: SessionId,
        descriptor: &SessionDescriptor,
    ) -> Result<(), SessionDescriptorError> {
        let lock = self.session_lock(session);
        let _guard = lock.lock().expect("descriptor session lock poisoned");
        self.write_sync(session, descriptor)
    }

    fn write_descriptor_if_absent(
        &self,
        session: SessionId,
        descriptor: &SessionDescriptor,
    ) -> Result<bool, SessionDescriptorError> {
        // Check and write are one step under this store's session lock, so
        // callers of this same store cannot interleave. The lock is
        // in-process: another process, or a second store over the same root,
        // still races -- the window is one acquisition wide, not zero.
        let lock = self.session_lock(session);
        let _guard = lock.lock().expect("descriptor session lock poisoned");
        if self.read_sync(session).is_some() {
            return Ok(false);
        }
        self.write_sync(session, descriptor)?;
        Ok(true)
    }

    fn update_descriptor(
        &self,
        session: SessionId,
        edit: &mut dyn FnMut(&mut SessionDescriptor),
    ) -> Result<DescriptorUpdate, SessionDescriptorError> {
        // The read and the write are inside one lock: that is the whole
        // point of the method. Taking it around the write alone would still
        // let a second caller read the pre-edit descriptor and write it back.
        let lock = self.session_lock(session);
        let _guard = lock.lock().expect("descriptor session lock poisoned");
        let Some(mut descriptor) = self.read_sync(session) else {
            return Ok(DescriptorUpdate::Absent);
        };
        edit(&mut descriptor);
        self.write_sync(session, &descriptor)?;
        Ok(DescriptorUpdate::Written)
    }

    fn delete_descriptor(&self, session: SessionId) {
        // Under the same lock as the writes: a delete landing between an
        // update's read and its write would otherwise be undone by that
        // write, resurrecting the descriptor of a torn-down session.
        let lock = self.session_lock(session);
        let _guard = lock.lock().expect("descriptor session lock poisoned");
        let dir = session_dir(&self.root, session);
        // Best-effort: a missing dir is not an error (idempotent teardown).
        drop(fs::remove_dir_all(&dir));
    }
}

/// In-memory descriptor store for tests.
pub struct InMemoryDescriptorStore {
    descriptors: Mutex<HashMap<SessionId, SessionDescriptor>>,
}

impl InMemoryDescriptorStore {
    pub fn new() -> Self {
        Self {
            descriptors: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryDescriptorStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionDescriptorStore for InMemoryDescriptorStore {
    fn read_descriptor(&self, session: SessionId) -> Option<SessionDescriptor> {
        self.descriptors
            .lock()
            .expect("descriptor mutex poisoned")
            .get(&session)
            .cloned()
    }

    fn write_descriptor(
        &self,
        session: SessionId,
        descriptor: &SessionDescriptor,
    ) -> Result<(), SessionDescriptorError> {
        self.descriptors
            .lock()
            .expect("descriptor mutex poisoned")
            .insert(session, descriptor.clone());
        Ok(())
    }

    fn write_descriptor_if_absent(
        &self,
        session: SessionId,
        descriptor: &SessionDescriptor,
    ) -> Result<bool, SessionDescriptorError> {
        // One lock acquisition spans the lookup and the insert, so nothing can
        // land an entry between them.
        let mut descriptors = self.descriptors.lock().expect("descriptor mutex poisoned");
        if descriptors.contains_key(&session) {
            return Ok(false);
        }
        descriptors.insert(session, descriptor.clone());
        Ok(true)
    }

    fn update_descriptor(
        &self,
        session: SessionId,
        edit: &mut dyn FnMut(&mut SessionDescriptor),
    ) -> Result<DescriptorUpdate, SessionDescriptorError> {
        // One lock acquisition spans the lookup and the edit, so the map
        // entry is mutated in place rather than read out and put back.
        let mut descriptors = self.descriptors.lock().expect("descriptor mutex poisoned");
        let Some(descriptor) = descriptors.get_mut(&session) else {
            return Ok(DescriptorUpdate::Absent);
        };
        edit(descriptor);
        Ok(DescriptorUpdate::Written)
    }

    fn delete_descriptor(&self, session: SessionId) {
        self.descriptors
            .lock()
            .expect("descriptor mutex poisoned")
            .remove(&session);
    }
}

/// Re-export the provenance variant constructors the composition root uses
/// when recording where a session came from.
pub fn fresh_provenance() -> SessionProvenance {
    SessionProvenance::Fresh
}

#[cfg(test)]
mod tests {
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
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("descriptor-test-{}-{n}", std::process::id()));
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
        std::thread::scope(|scope| {
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
}
