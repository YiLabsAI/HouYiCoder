//! Session metadata sidecar store: the disk + in-memory impls of the
//! SessionMetaStore trait from the context layer. The disk impl writes
//! <root>/<sid>/session.json (atomic tmp+rename, 0o600, dir 0o700) alongside
//! the log.jsonl the file backend owns. The in-memory impl serves the test
//! tier so unit tests never touch the real home sessions dir.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use houyicoder_context::{
    ContextMetaError, SessionId, SessionMeta, SessionMetaStore, SessionProvenance,
};

/// Disk-backed meta store. Writes <root>/<sid>/session.json atomically. The
/// root matches the file backend's root (the composition root passes the same
/// session_log_root), so session.json + log.jsonl share a per-session dir.
pub struct FileMetaStore {
    root: PathBuf,
}

impl FileMetaStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
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
        // path would read a truncated JSON + fail to parse).
        let tmp = dir.join("session.json.tmp");
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
        self.write_sync(session, meta)
    }

    fn delete_meta(&self, session: SessionId) {
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
