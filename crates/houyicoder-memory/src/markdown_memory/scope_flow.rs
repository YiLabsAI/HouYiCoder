//! Scope changes for the markdown memory provider: promoting a topic into
//! the explicit project scope and demoting it back into the automatic scope.
//! Both move the record between roots and rebuild the derived indexes inside
//! one critical section held across processes.

use std::fs;
use std::fs::OpenOptions;
use std::path::Path;
use std::time::SystemTime;

use houyicoder_context::MemoryError;
use houyicoder_context::MemoryScope;

use super::MarkdownMemoryProvider;
use super::io::relocate_record;
use super::io::sanitize_key;

/// Stamp a file as freshly modified. Promotion is an edit of the record's
/// standing, so the file it lands in carries the newest time; a failure is
/// only a lost tie-break, never a lost write.
fn touch_file(path: &Path) {
    let file = match OpenOptions::new().append(true).open(path) {
        Ok(file) => file,
        Err(e) => {
            tracing::debug!("memory promote: {} not stamped: {e}", path.display());
            return;
        }
    };
    if let Err(e) = file.set_modified(SystemTime::now()) {
        tracing::debug!("memory promote: {} not stamped: {e}", path.display());
    }
}

impl MarkdownMemoryProvider {
    /// Promote a topic into the project scope, then rebuild both derived
    /// indexes. Repeated promotion is idempotent.
    pub(super) fn promote_impl(&self, key: &str) -> Result<(), MemoryError> {
        let key = sanitize_key(key)?;
        // Keep topic and index changes in one critical section, across
        // processes as well as within this one.
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let _root_locks = self.lock_roots()?;
        let auto_root = self.write_root().to_path_buf();
        let project_root = self.root_for_scope(MemoryScope::Project).to_path_buf();
        let src_topic = auto_root.join(format!("{key}.md"));
        let dst_topic = project_root.join(format!("{key}.md"));
        let exists_in_auto = src_topic.is_file();
        let exists_in_project = dst_topic.is_file();
        if !exists_in_auto && !exists_in_project {
            return Err(MemoryError::NotFound);
        }
        // Reconcile the topic files across the two roots. When both roots
        // hold a copy, the project copy is the explicit source of truth, so
        // the auto copy goes. When only the auto copy exists, it moves into
        // the project root. When only the project copy exists, the move
        // changes nothing, which makes a repeated promote idempotent.
        fs::create_dir_all(&project_root).map_err(|_| MemoryError::Io)?;
        if exists_in_auto && exists_in_project {
            if fs::remove_file(&src_topic).is_err() {
                tracing::warn!("memory promote: auto copy of {key} left in place");
            }
        } else if exists_in_auto && fs::rename(&src_topic, &dst_topic).is_err() {
            // A rename within one filesystem is atomic. Across devices it
            // fails, and the fallback writes the destination atomically
            // before removing the source.
            relocate_record(&src_topic, &dst_topic)?;
        }
        // A promoted topic is explicit from now on. Its modification time is
        // stamped so the promotion itself is the newest edit, which keeps the
        // project copy ahead of any auto copy a later dream would write.
        touch_file(&dst_topic);
        // Regenerate both indexes under the same lock, so the auto root loses
        // the pointer and the project root gains it.
        drop(self.rebuild_index_impl_locked());
        Ok(())
    }

    /// Demote a topic into the auto scope and rebuild both derived indexes.
    /// Repeated demotion is idempotent.
    pub(super) fn demote_impl(&self, key: &str) -> Result<(), MemoryError> {
        let key = sanitize_key(key)?;
        // Keep topic and index changes in one critical section, across
        // processes as well as within this one.
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let _root_locks = self.lock_roots()?;
        let auto_root = self.write_root().to_path_buf();
        let project_root = self.root_for_scope(MemoryScope::Project).to_path_buf();
        let src_topic = project_root.join(format!("{key}.md"));
        let dst_topic = auto_root.join(format!("{key}.md"));
        let exists_in_project = src_topic.is_file();
        let exists_in_auto = dst_topic.is_file();
        if !exists_in_project && !exists_in_auto {
            return Err(MemoryError::NotFound);
        }
        // Reconcile the topic files. When both roots hold a copy, the auto
        // copy is the recall-on-demand target, so the project copy goes and
        // the always-on scope no longer carries the topic. When only the
        // project copy exists, it moves into the auto root. When only the
        // auto copy exists, the move changes nothing, which makes a repeated
        // demote idempotent.
        fs::create_dir_all(&auto_root).map_err(|_| MemoryError::Io)?;
        if exists_in_project && exists_in_auto {
            if fs::remove_file(&src_topic).is_err() {
                tracing::warn!("memory demote: project copy of {key} left in place");
            }
        } else if exists_in_project && fs::rename(&src_topic, &dst_topic).is_err() {
            // A rename within one filesystem is atomic. Across devices it
            // fails, and the fallback writes the destination atomically
            // before removing the source.
            relocate_record(&src_topic, &dst_topic)?;
        }
        // Step two: regenerate both indexes under the same lock.
        drop(self.rebuild_index_impl_locked());
        Ok(())
    }
}
