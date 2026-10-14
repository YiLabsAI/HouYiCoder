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

#[cfg(unix)]
use fs::DirBuilder;
use fs::OpenOptions;
use houyicoder_context::{
    DescriptorUpdate, SessionDescriptor, SessionDescriptorError, SessionDescriptorStore, SessionId,
    SessionProvenance,
    session_class::{DESCRIPTOR_FILE, session_dir},
};
#[cfg(test)]
use std::env::temp_dir;
use std::process::id;
#[cfg(test)]
use std::thread::scope;

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
            DirBuilder::new()
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
            id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        {
            #[cfg(unix)]
            let mut opts = {
                let mut o = OpenOptions::new();
                o.write(true).create(true).truncate(true).mode(0o600);
                o
            };
            #[cfg(not(unix))]
            let mut opts = OpenOptions::new();
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
mod tests;
