//! Session metadata sidecar store: the disk + in-memory impls of the
//! SessionMetaStore trait from the context layer. The disk impl writes
//! <root>/<sid>/session.json (atomic tmp+rename, 0o600, dir 0o700) alongside
//! the log.jsonl the file backend owns. The in-memory impl serves the test
//! tier so unit tests never touch the real home sessions dir.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use houyicoder_context::{
    ContextMetaError, MetaUpdate, SessionId, SessionMeta, SessionMetaStore, SessionProvenance,
};

/// Disk-backed meta store. Writes <root>/<sid>/session.json atomically. The
/// root matches the file backend's root (the composition root passes the same
/// session_log_root), so session.json + log.jsonl share a per-session dir.
pub struct FileMetaStore {
    root: PathBuf,
    /// One lock per session, guarding a whole write or read-modify-write so
    /// two callers touching the same sidecar serialize instead of each
    /// publishing a copy derived from the state it read. Per session, not
    /// one lock for the store: the write ends in an fsync, and one session's
    /// slow flush must not stall an unrelated session's write. Entries are
    /// never evicted - removing one while another thread holds it would hand
    /// the next caller a fresh lock and silently drop the exclusion - so the
    /// map grows with the number of distinct sessions touched in a process,
    /// which is the session count, not a leak that tracks time.
    locks: Mutex<HashMap<SessionId, Arc<Mutex<()>>>>,
}

impl FileMetaStore {
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
                .expect("meta lock map poisoned")
                .entry(session)
                .or_default(),
        )
    }

    fn session_dir(&self, session: SessionId) -> PathBuf {
        self.root.join(format!("{session}"))
    }

    fn meta_path(&self, session: SessionId) -> PathBuf {
        self.session_dir(session).join("session.json")
    }

    fn ensure_dir(path: &Path) -> Result<(), ContextMetaError> {
        #[cfg(unix)]
        {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|e| ContextMetaError(format!("mkdir {path:?}: {e}")))
        }
        #[cfg(not(unix))]
        {
            fs::create_dir_all(path).map_err(|e| ContextMetaError(format!("mkdir {path:?}: {e}")))
        }
    }

    fn write_sync(&self, session: SessionId, meta: &SessionMeta) -> Result<(), ContextMetaError> {
        let dir = self.session_dir(session);
        Self::ensure_dir(&dir)?;
        let path = self.meta_path(session);
        let body = serde_json::to_vec_pretty(meta)
            .map_err(|e| ContextMetaError(format!("serialize meta: {e}")))?;
        // Atomic: write to a sibling tmp file, then rename over the target so
        // a crash mid-write cannot leave a half-written sidecar (the resume
        // path would read a truncated JSON + fail to parse). The tmp name is
        // unique per write: sharing one name lets a writer rename a file
        // holding another's half-written bytes, and a corrupt sidecar reads
        // back as absent, so the cwd and model vanish silently. The pid
        // covers a second process on the same root, which no in-process lock
        // reaches; the counter covers two store instances in one process,
        // holding separate lock maps. A crash between create and rename now
        // leaves an orphan, pruned with the session dir - no reaper needed.
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
                .map_err(|e| ContextMetaError(format!("open tmp {tmp:?}: {e}")))?;
            f.write_all(&body)
                .map_err(|e| ContextMetaError(format!("write tmp {tmp:?}: {e}")))?;
            f.sync_all()
                .map_err(|e| ContextMetaError(format!("sync tmp {tmp:?}: {e}")))?;
        }
        fs::rename(&tmp, &path)
            .map_err(|e| ContextMetaError(format!("rename {tmp:?} -> {path:?}: {e}")))
    }

    fn read_sync(&self, session: SessionId) -> Option<SessionMeta> {
        let path = self.meta_path(session);
        let body = fs::read_to_string(&path).ok()?;
        // A truncated/corrupt sidecar is tolerated as absent rather than
        // fatal: the resume path falls back to deriving cwd/model from the
        // current config, which is safer than refusing to start.
        serde_json::from_str(&body).ok()
    }

    fn list_sync(&self) -> Vec<(SessionId, SessionMeta)> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let out: Vec<(SessionId, SessionMeta)> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let sid_str = e.file_name().to_string_lossy().into_owned();
                let sid = SessionId::from_display_string(&sid_str)?;
                let meta = self.read_sync(sid)?;
                Some((sid, meta))
            })
            .collect();
        // Unsorted: callers order by log.jsonl mtime (last activity), not the
        // sidecar's updated_at (a creation-time proxy that never bumps).
        out
    }
}

impl SessionMetaStore for FileMetaStore {
    fn read_meta(&self, session: SessionId) -> Option<SessionMeta> {
        self.read_sync(session)
    }

    fn write_meta(&self, session: SessionId, meta: &SessionMeta) -> Result<(), ContextMetaError> {
        let lock = self.session_lock(session);
        let _guard = lock.lock().expect("meta session lock poisoned");
        self.write_sync(session, meta)
    }

    fn update_meta(
        &self,
        session: SessionId,
        edit: &mut dyn FnMut(&mut SessionMeta),
    ) -> Result<MetaUpdate, ContextMetaError> {
        // The read and the write are inside one lock: that is the whole
        // point of the method. Taking it around the write alone would still
        // let a second caller read the pre-edit sidecar and write it back.
        let lock = self.session_lock(session);
        let _guard = lock.lock().expect("meta session lock poisoned");
        let Some(mut meta) = self.read_sync(session) else {
            return Ok(MetaUpdate::Absent);
        };
        edit(&mut meta);
        self.write_sync(session, &meta)?;
        Ok(MetaUpdate::Written)
    }

    fn delete_meta(&self, session: SessionId) {
        // Under the same lock as the writes: a delete landing between an
        // update's read and its write would otherwise be undone by that
        // write, resurrecting the sidecar of a torn-down session.
        let lock = self.session_lock(session);
        let _guard = lock.lock().expect("meta session lock poisoned");
        let dir = self.session_dir(session);
        // Best-effort: a missing dir is not an error (idempotent teardown).
        drop(fs::remove_dir_all(&dir));
    }

    fn list_metas(&self) -> Vec<(SessionId, SessionMeta)> {
        self.list_sync()
    }
}

/// In-memory meta store for the test tier. Never touches disk. Backed by a
/// HashMap so unit + integration tests read/write/list without polluting the
/// developer home sessions dir.
pub struct InMemoryMetaStore {
    metas: Mutex<HashMap<SessionId, SessionMeta>>,
}

impl InMemoryMetaStore {
    pub fn new() -> Self {
        Self {
            metas: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryMetaStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionMetaStore for InMemoryMetaStore {
    fn read_meta(&self, session: SessionId) -> Option<SessionMeta> {
        self.metas
            .lock()
            .expect("meta mutex poisoned")
            .get(&session)
            .cloned()
    }

    fn write_meta(&self, session: SessionId, meta: &SessionMeta) -> Result<(), ContextMetaError> {
        self.metas
            .lock()
            .expect("meta mutex poisoned")
            .insert(session, meta.clone());
        Ok(())
    }

    fn update_meta(
        &self,
        session: SessionId,
        edit: &mut dyn FnMut(&mut SessionMeta),
    ) -> Result<MetaUpdate, ContextMetaError> {
        // One lock acquisition spans the lookup and the edit, so the map
        // entry is mutated in place rather than read out and put back.
        let mut metas = self.metas.lock().expect("meta mutex poisoned");
        let Some(meta) = metas.get_mut(&session) else {
            return Ok(MetaUpdate::Absent);
        };
        edit(meta);
        Ok(MetaUpdate::Written)
    }

    fn delete_meta(&self, session: SessionId) {
        self.metas
            .lock()
            .expect("meta mutex poisoned")
            .remove(&session);
    }

    fn list_metas(&self) -> Vec<(SessionId, SessionMeta)> {
        let out: Vec<(SessionId, SessionMeta)> = self
            .metas
            .lock()
            .expect("meta mutex poisoned")
            .iter()
            .map(|(s, m)| (*s, m.clone()))
            .collect();
        out
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

    fn sample_meta(name: Option<&str>, ts: u64) -> SessionMeta {
        SessionMeta {
            name: name.map(str::to_string),
            name_source: NameSource::Auto,
            cwd: "/repo".into(),
            model: "test".into(),
            provenance: SessionProvenance::Fresh,
            version: "test".into(),
            created_at: ts,
        }
    }

    fn temp_root() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("meta-test-{}-{n}", std::process::id()));
        fs::create_dir_all(&p).expect("mkdir root");
        p
    }

    #[test]
    fn test_file_round_trips_meta() {
        let root = temp_root();
        let store = FileMetaStore::new(root.clone());
        let sid = SessionId::new();
        let meta = sample_meta(Some("my session"), 100);
        store.write_meta(sid, &meta).expect("write");
        let back = store.read_meta(sid).expect("read");
        assert_eq!(back, meta, "meta round-trips through disk");
        let listed = store.list_metas();
        assert_eq!(listed.len(), 1, "one session listed");
        assert_eq!(listed[0].1.name.as_deref(), Some("my session"));
        drop(fs::remove_dir_all(&root));
    }

    /// Concurrent edits to one sidecar all survive. Each update appends a
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
        let store = FileMetaStore::new(root.clone());
        let sid = SessionId::new();
        store
            .write_meta(sid, &sample_meta(Some(""), 1))
            .expect("seed");
        std::thread::scope(|scope| {
            for _ in 0..WRITERS {
                scope.spawn(|| {
                    for _ in 0..EDITS {
                        store
                            .update_meta(sid, &mut |meta| {
                                meta.name.get_or_insert_with(String::new).push('x');
                            })
                            .expect("update");
                    }
                });
            }
        });
        let back = store.read_meta(sid).expect("read");
        assert_eq!(
            back.name.as_deref().map(str::len),
            Some(WRITERS * EDITS),
            "every concurrent edit should survive, none overwritten"
        );
        drop(fs::remove_dir_all(&root));
    }

    /// An update against a session with no sidecar reports Absent rather
    /// than creating one. A sidecar materializes on the first durable
    /// append; an update is an edit to an existing descriptor, so a rename
    /// before that point must not mint a sidecar with default fields.
    #[test]
    fn test_update_absent_writes_nothing() {
        let root = temp_root();
        let store = FileMetaStore::new(root.clone());
        let sid = SessionId::new();
        let mut ran = false;
        let outcome = store
            .update_meta(sid, &mut |_| ran = true)
            .expect("update should not error on a missing sidecar");
        assert_eq!(outcome, MetaUpdate::Absent, "no sidecar -> Absent");
        assert!(!ran, "the edit closure should not run without a sidecar");
        assert!(store.read_meta(sid).is_none(), "no sidecar was created");
        drop(fs::remove_dir_all(&root));
    }

    #[test]
    fn test_file_lists_all_sessions() {
        let root = temp_root();
        let store = FileMetaStore::new(root.clone());
        let older = SessionId::new();
        let newer = SessionId::new();
        store
            .write_meta(older, &sample_meta(None, 100))
            .expect("write older");
        store
            .write_meta(newer, &sample_meta(None, 200))
            .expect("write newer");
        let listed = store.list_metas();
        assert_eq!(listed.len(), 2, "two sessions listed");
        // list_metas is unsorted (callers order by log.jsonl mtime, not the
        // sidecar's created_at); assert presence, not order.
        let sids: Vec<_> = listed.iter().map(|(s, _)| *s).collect();
        assert!(
            sids.contains(&older) && sids.contains(&newer),
            "both listed: {sids:?}"
        );
        drop(fs::remove_dir_all(&root));
    }

    #[test]
    fn test_file_store_delete_idempotent() {
        let root = temp_root();
        let store = FileMetaStore::new(root.clone());
        let sid = SessionId::new();
        store.delete_meta(sid); // missing dir: no panic, no error
        store.write_meta(sid, &sample_meta(None, 1)).expect("write");
        store.delete_meta(sid);
        assert!(store.read_meta(sid).is_none(), "deleted -> absent");
        drop(fs::remove_dir_all(&root));
    }

    #[test]
    fn test_file_store_read_absent() {
        let root = temp_root();
        let store = FileMetaStore::new(root.clone());
        let sid = SessionId::new();
        assert!(store.read_meta(sid).is_none(), "no sidecar -> None");
        drop(fs::remove_dir_all(&root));
    }

    /// The in-memory store honors the same update contract: an edit applies
    /// in place and reports Written, a missing session reports Absent. The
    /// test tier runs against this impl, so a divergence here would let a
    /// caller pass its tests and lose an edit on disk.
    #[test]
    fn test_in_memory_update_applies() {
        let store = InMemoryMetaStore::new();
        let sid = SessionId::new();
        assert_eq!(
            store.update_meta(sid, &mut |_| ()).expect("absent update"),
            MetaUpdate::Absent,
            "no entry -> Absent"
        );
        store
            .write_meta(sid, &sample_meta(Some("before"), 1))
            .expect("write");
        let outcome = store
            .update_meta(sid, &mut |meta| meta.name = Some("after".into()))
            .expect("update");
        assert_eq!(outcome, MetaUpdate::Written, "entry present -> Written");
        assert_eq!(
            store.read_meta(sid).and_then(|m| m.name).as_deref(),
            Some("after"),
            "the edit is visible on the next read"
        );
    }

    #[test]
    fn test_in_memory_round_trips() {
        let store = InMemoryMetaStore::new();
        let sid = SessionId::new();
        let meta = sample_meta(Some("x"), 5);
        store.write_meta(sid, &meta).expect("write");
        assert_eq!(store.read_meta(sid), Some(meta));
        assert_eq!(store.list_metas().len(), 1);
        store.delete_meta(sid);
        assert!(store.read_meta(sid).is_none());
    }
}
