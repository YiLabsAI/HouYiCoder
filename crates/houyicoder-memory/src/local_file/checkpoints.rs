//! Checkpoint manifests: one file per checkpoint under the session's
//! checkpoint directory, written atomically.
//!
//! A manifest is published by writing a temp sibling, fsyncing it, and
//! renaming over the target, so a crash leaves either the previous complete
//! file or the new complete file, never a truncated one. The directory entry
//! gets its own fsync because the rename's durability needs it.

use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use houyicoder_context::session_class::CHECKPOINT_DIR;
use houyicoder_context::{CheckpointId, CheckpointManifest, ContextError, SessionId};

use super::LocalFileBackend;

impl LocalFileBackend {
    pub(super) fn write_checkpoint_sync(
        &self,
        manifest: CheckpointManifest,
    ) -> Result<CheckpointId, ContextError> {
        let session = manifest.session;
        let id = manifest.id;
        Self::ensure_dir(&self.checkpoint_dir(session))?;
        let path = self.checkpoint_path(session, id);
        let json = serde_json::to_string(&manifest)
            .map_err(|_| ContextError::Corrupt("manifest failed to serialize".into()))?;
        // Atomic write: serialize to a temp sibling, fsync, rename over the
        // target so a crash never leaves a partial/truncated manifest (a
        // reader sees either the old checkpoint or the complete new one).
        let tmp = path.with_extension("tmp");
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        opts.mode(0o600);
        let mut f = opts.open(&tmp).map_err(|_| ContextError::Io)?;
        f.write_all(json.as_bytes()).map_err(|_| ContextError::Io)?;
        f.sync_all().map_err(|_| ContextError::Io)?;
        // fsync the directory so the rename is durable (POSIX rename is
        // atomic but the dir entry needs its own fsync).
        drop(f);
        fs::rename(&tmp, &path).map_err(|_| ContextError::Io)?;
        if let Some(dir) = path.parent()
            && let Ok(dir_file) = File::open(dir)
        {
            drop(dir_file.sync_all());
        }
        Ok(id)
    }

    pub(super) fn read_checkpoint_sync(
        &self,
        id: CheckpointId,
    ) -> Result<CheckpointManifest, ContextError> {
        // Checkpoints are namespaced by session; find the file by scanning the
        // root's session dirs for <session>/checkpoints/<id>.json.
        let entries = fs::read_dir(&self.root).map_err(|_| ContextError::Io)?;
        for entry in entries.flatten() {
            let cp_dir = entry.path().join(CHECKPOINT_DIR);
            let cp_file = cp_dir.join(format!("{id}.json"));
            if cp_file.exists() {
                let content = fs::read_to_string(&cp_file).map_err(|_| ContextError::Io)?;
                return serde_json::from_str(&content)
                    .map_err(|e| ContextError::Corrupt(format!("bad checkpoint: {e}")));
            }
        }
        Err(ContextError::NotFound)
    }

    pub(super) fn list_checkpoints_sync(
        &self,
        session: SessionId,
    ) -> Result<Vec<CheckpointId>, ContextError> {
        let dir = self.checkpoint_dir(session);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(ContextError::Io),
        };
        let mut ids: Vec<CheckpointId> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_str()?;
                let stem = name.strip_suffix(".json")?;
                CheckpointId::from_display_string(stem)
            })
            .collect();
        ids.sort();
        Ok(ids)
    }
}
