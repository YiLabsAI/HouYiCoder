//! Scope changes for the markdown memory provider: promoting a topic into
//! the explicit project scope and demoting it back into the automatic scope.
//! Both move the record between roots and rebuild the derived indexes inside
//! one critical section held across processes.

use std::collections::HashSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::Path;
use std::time::SystemTime;

use houyicoder_api::agent_event::{MemoryChange, MemoryOperation};
use houyicoder_api::memory::MemoryWriteOutcome;
use houyicoder_context::{
    MemoryEntry, MemoryError, MemoryOrigin, MemoryScope, MemorySummary, MemoryType,
};

use super::MarkdownMemoryProvider;
use super::io::{
    StatRecord, list_memories_impl, relocate_record, sanitize_key, validate_index_key,
};

const ARCHIVE_DIR: &str = "archive";

fn move_topic(src: &Path, dst: &Path) -> Result<(), MemoryError> {
    if fs::rename(src, dst).is_err() {
        relocate_record(src, dst)?;
    }
    Ok(())
}

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
        // Both indexes describe this key and both change now. Mark them before
        // the topic moves, so a publish that fails leaves the next session a
        // reason to repair both roots instead of one stale row each way.
        Self::mark_index_dirty(&auto_root)?;
        Self::mark_index_dirty(&project_root)?;
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
        // the project root no longer carries the topic. When only the
        // project copy exists, it moves into the auto root. When only the
        // auto copy exists, the move changes nothing, which makes a repeated
        // demote idempotent.
        fs::create_dir_all(&auto_root).map_err(|_| MemoryError::Io)?;
        // The project index loses this row and the automatic index gains it.
        // Both markers go down before the move, so a failed publish is
        // repaired on the next start.
        Self::mark_index_dirty(&auto_root)?;
        Self::mark_index_dirty(&project_root)?;
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

    pub(super) fn archive_impl(&self, key: &str, scope: MemoryScope) -> Result<(), MemoryError> {
        let key = sanitize_key(key)?;
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let _root_locks = self.lock_roots()?;
        let topic_root = self.root_for_scope(scope).to_path_buf();
        let topic_path = topic_root.join(format!("{key}.md"));
        if !topic_path.is_file() {
            return Err(MemoryError::NotFound);
        }
        let archive_dir = topic_root.join(ARCHIVE_DIR);
        fs::create_dir_all(&archive_dir).map_err(|_| MemoryError::Io)?;
        Self::mark_index_dirty(&topic_root)?;
        move_topic(&topic_path, &archive_dir.join(format!("{key}.md")))?;
        // Keep the stats row intact so restoring this archived entry recovers
        // its accumulated recall hits, violations, and occurrence count.
        drop(self.rebuild_index_impl_locked());
        Ok(())
    }

    pub(super) fn restore_impl(&self, key: &str, scope: MemoryScope) -> Result<(), MemoryError> {
        let key = sanitize_key(key)?;
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let _root_locks = self.lock_roots()?;
        let topic_root = self.root_for_scope(scope).to_path_buf();
        let archive_path = topic_root.join(ARCHIVE_DIR).join(format!("{key}.md"));
        if !archive_path.is_file() {
            return Err(MemoryError::NotFound);
        }
        fs::create_dir_all(&topic_root).map_err(|_| MemoryError::Io)?;
        let dst_path = topic_root.join(format!("{key}.md"));
        Self::mark_index_dirty(&topic_root)?;
        move_topic(&archive_path, &dst_path)?;
        touch_file(&dst_path);
        drop(self.rebuild_index_impl_locked());
        Ok(())
    }

    pub(super) fn delete_archived_impl(
        &self,
        key: &str,
        scope: MemoryScope,
    ) -> Result<(), MemoryError> {
        let key = sanitize_key(key)?;
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let _root_locks = self.lock_roots()?;
        let archive_path = self
            .root_for_scope(scope)
            .join(ARCHIVE_DIR)
            .join(format!("{key}.md"));
        match fs::remove_file(&archive_path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(MemoryError::NotFound);
            }
            Err(_) => return Err(MemoryError::Io),
        }
        if !self.key_exists_anywhere(&key) {
            let mut stats = self.load_stats();
            if stats.remove(&key).is_some() {
                self.save_stats(&stats);
            }
        }
        Ok(())
    }

    pub(super) fn list_archived_impl(&self) -> Vec<MemorySummary> {
        let mut out: Vec<MemorySummary> = Vec::new();
        let mut seen: HashSet<(String, usize)> = HashSet::new();
        for (root_index, root) in self.roots.iter().enumerate() {
            let archive_dir = root.join(ARCHIVE_DIR);
            for topic in Self::scan_root(&archive_dir) {
                if !seen.insert((topic.key.clone(), root_index)) {
                    continue;
                }
                let scope = if self.roots.len() == 1 {
                    MemoryScope::Auto
                } else {
                    MemoryScope::for_root_index(root_index)
                };
                if let Ok(fm) = self.read_frontmatter(&topic.path) {
                    out.push(
                        MemorySummary::new(
                            topic.key,
                            fm.description,
                            fm.memory_type,
                            scope,
                            topic.mtime,
                        )
                        .with_origin(fm.origin),
                    );
                }
            }
        }
        out.sort_by(|a, b| {
            b.mtime_secs
                .cmp(&a.mtime_secs)
                .then_with(|| a.key.cmp(&b.key))
        });
        out
    }

    pub(super) fn show_archived_impl(&self, key: &str, scope: MemoryScope) -> Option<MemoryEntry> {
        let key = sanitize_key(key).ok()?;
        let path = self
            .root_for_scope(scope)
            .join(ARCHIVE_DIR)
            .join(format!("{key}.md"));
        self.read_topic(&path).ok()
    }

    pub(super) fn replace_in_scope_impl(
        &self,
        mut entry: MemoryEntry,
        scope: MemoryScope,
        replaces: &[String],
    ) -> Result<MemoryWriteOutcome, MemoryError> {
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        entry.key = sanitize_key(&entry.key)?;
        validate_index_key(&entry.key)?;
        if entry.origin.is_background() && scope == MemoryScope::User && self.roots.len() > 1 {
            return Err(MemoryError::InvalidPath(
                "background writer cannot target user scope".into(),
            ));
        }
        let target_root = self.root_for_scope(scope).to_path_buf();
        let outcome = self.add_if_changed_in_root(&target_root, &entry)?;
        if replaces.is_empty() {
            return Ok(outcome);
        }
        let _root_locks = self.lock_roots()?;
        let allowed_scopes: &[MemoryScope] = if entry.origin.is_background() {
            if scope == MemoryScope::Project {
                &[MemoryScope::Project, MemoryScope::Auto]
            } else {
                &[MemoryScope::Auto]
            }
        } else {
            match scope {
                MemoryScope::User => &[MemoryScope::User, MemoryScope::Auto],
                MemoryScope::Project => &[MemoryScope::Project, MemoryScope::Auto],
                MemoryScope::Auto => &[MemoryScope::Auto],
            }
        };
        let mut stats = self.load_stats();
        let mut acc_hits = 0u64;
        let mut acc_violations = 0u64;
        let mut acc_occurrences = 0u64;
        let mut archived_any = false;
        let mut seen = HashSet::new();
        for old in replaces {
            let Ok(clean) = sanitize_key(old) else {
                continue;
            };
            // Never archive the key just written when a model lists the target
            // key inside its own replacement list.
            if clean == entry.key || !seen.insert(clean.clone()) {
                continue;
            }
            let mut moved_this_key = false;
            for &cand_scope in allowed_scopes {
                if entry.origin.is_background()
                    && cand_scope == MemoryScope::User
                    && self.roots.len() > 1
                {
                    continue;
                }
                let cand_root = self.root_for_scope(cand_scope).to_path_buf();
                let cand_path = cand_root.join(format!("{clean}.md"));
                if !cand_path.is_file() {
                    continue;
                }
                if entry.origin.is_background()
                    && let Ok(fm) = self.read_frontmatter(&cand_path)
                    && matches!(fm.origin, MemoryOrigin::User | MemoryOrigin::PrimaryAgent)
                {
                    continue;
                }
                let archive_dir = cand_root.join(ARCHIVE_DIR);
                if fs::create_dir_all(&archive_dir).is_ok() {
                    drop(Self::mark_index_dirty(&cand_root));
                    if move_topic(&cand_path, &archive_dir.join(format!("{clean}.md"))).is_ok() {
                        moved_this_key = true;
                        archived_any = true;
                    }
                }
            }
            if moved_this_key {
                let st = stats.remove(&clean);
                acc_hits = acc_hits.saturating_add(st.as_ref().map_or(0, |s| s.recall_hits));
                acc_violations =
                    acc_violations.saturating_add(st.as_ref().map_or(0, |s| s.gate_violations));
                acc_occurrences =
                    acc_occurrences.saturating_add(st.as_ref().map_or(1, |s| s.occurrences.max(1)));
            }
        }
        if acc_hits > 0 || acc_violations > 0 || acc_occurrences > 0 {
            let target_stat = stats.entry(entry.key.clone()).or_default();
            target_stat.occurrences = target_stat.occurrences.max(1);
            target_stat.recall_hits = target_stat.recall_hits.saturating_add(acc_hits);
            target_stat.gate_violations =
                target_stat.gate_violations.saturating_add(acc_violations);
            target_stat.occurrences = target_stat.occurrences.saturating_add(acc_occurrences);
            self.save_stats(&stats);
        }
        if archived_any {
            drop(self.rebuild_index_impl_locked());
        }
        Ok(outcome)
    }

    /// Single-pass deterministic scope promotion, demotion, decay archival, and
    /// orphan-stat cleanup. Rebuilds affected indexes at most once.
    pub(super) fn reconcile_scopes_impl(&self, now_secs: u64) -> Vec<MemoryChange> {
        let _guard = self.write_lock.lock().expect("write lock poisoned");
        let Ok(_root_locks) = self.lock_roots() else {
            return Vec::new();
        };
        let summaries = list_memories_impl(self);
        let mut stats = self.load_stats();
        let multi_scope = self.roots.len() > 1;
        let auto_root = self.write_root().to_path_buf();
        let project_root = self.root_for_scope(MemoryScope::Project).to_path_buf();
        let mut changes = Vec::new();

        let auto_expire = self.auto_expire;
        for s in &summaries {
            if let Some(change) = Self::reconcile_entry(
                s,
                stats.get(&s.key),
                now_secs,
                multi_scope,
                auto_expire,
                &auto_root,
                &project_root,
            ) {
                changes.push(change);
            }
        }

        // Prune orphan stats keys that exist neither in active roots nor in archive.
        let before_stats = stats.len();
        stats.retain(|key, _| self.key_exists_anywhere(key));
        if stats.len() != before_stats {
            self.save_stats(&stats);
        }
        if !changes.is_empty() {
            drop(self.rebuild_index_impl_locked());
        }
        changes
    }

    fn reconcile_entry(
        s: &MemorySummary,
        stat: Option<&StatRecord>,
        now_secs: u64,
        multi_scope: bool,
        auto_expire: bool,
        auto_root: &Path,
        project_root: &Path,
    ) -> Option<MemoryChange> {
        let effective_scope = if multi_scope {
            s.scope
        } else {
            MemoryScope::Auto
        };
        let violations = stat.map(|r| r.gate_violations).unwrap_or(0);
        let occurrences = stat.map(|r| r.occurrences).unwrap_or(1);
        let hits = stat.map(|r| r.recall_hits).unwrap_or(0);
        let last_access = stat.map(|r| r.last_access_ts).unwrap_or(0);
        let unmodified_days = now_secs.saturating_sub(s.mtime_secs) / 86_400;
        let unrecalled_days = if last_access == 0 {
            unmodified_days
        } else {
            now_secs.saturating_sub(last_access) / 86_400
        };

        // Promotion:
        // Auto-scoped feedback rules with repeated violations/hits/occurrences promote to project scope.
        // Auto-scoped project facts with repeated recall hits (>= 8) or occurrences (>= 2) also promote
        // to project scope, ensuring high-frequency durable project facts survive and prioritize in the memory index
        // without crowding out active rules (which exclusively admits feedback rules).
        let eligible_for_promotion = match s.memory_type {
            MemoryType::Feedback => violations >= 2 || occurrences >= 2 || hits >= 8,
            MemoryType::Project => hits >= 8 || occurrences >= 2,
            _ => false,
        };

        if multi_scope && effective_scope == MemoryScope::Auto && eligible_for_promotion {
            let src = auto_root.join(format!("{}.md", s.key));
            let dst = project_root.join(format!("{}.md", s.key));
            if src.is_file() && fs::create_dir_all(project_root).is_ok() {
                drop(Self::mark_index_dirty(auto_root));
                drop(Self::mark_index_dirty(project_root));
                let moved = if dst.is_file() {
                    fs::remove_file(&src).is_ok()
                } else {
                    move_topic(&src, &dst).is_ok()
                };
                if moved {
                    touch_file(&dst);
                    return Some(MemoryChange {
                        key: s.key.clone(),
                        operation: MemoryOperation::Promoted,
                        scope: MemoryScope::Project,
                    });
                }
            }
        }

        if multi_scope
            && effective_scope == MemoryScope::Project
            && !matches!(s.origin, MemoryOrigin::User | MemoryOrigin::PrimaryAgent)
            && unmodified_days >= 60
            && unrecalled_days >= 60
            && violations == 0
            && occurrences <= 1
            && hits == 0
        {
            let src = project_root.join(format!("{}.md", s.key));
            let dst = auto_root.join(format!("{}.md", s.key));
            if src.is_file() && fs::create_dir_all(auto_root).is_ok() {
                drop(Self::mark_index_dirty(auto_root));
                drop(Self::mark_index_dirty(project_root));
                let moved = if dst.is_file() {
                    fs::remove_file(&src).is_ok()
                } else {
                    move_topic(&src, &dst).is_ok()
                };
                if moved {
                    return Some(MemoryChange {
                        key: s.key.clone(),
                        operation: MemoryOperation::Demoted,
                        scope: MemoryScope::Auto,
                    });
                }
            }
        }

        // Time-based decay expiration:
        // Gated on auto_expire (default false) to prevent silent data loss based on unproven day thresholds.
        // Replaces in write_or_replace_in_scope always deterministically archives superseded copies.
        if auto_expire
            && effective_scope == MemoryScope::Auto
            && !matches!(s.origin, MemoryOrigin::User | MemoryOrigin::PrimaryAgent)
        {
            let expired = match s.memory_type {
                MemoryType::Project => unmodified_days >= 21 && unrecalled_days >= 30,
                MemoryType::Feedback => {
                    unmodified_days >= 60 && unrecalled_days >= 60 && violations == 0
                }
                MemoryType::Reference => unmodified_days >= 90 && unrecalled_days >= 90,
                MemoryType::User => false,
            };
            if expired {
                let src = auto_root.join(format!("{}.md", s.key));
                let archive_dir = auto_root.join(ARCHIVE_DIR);
                if src.is_file() && fs::create_dir_all(&archive_dir).is_ok() {
                    drop(Self::mark_index_dirty(auto_root));
                    if move_topic(&src, &archive_dir.join(format!("{}.md", s.key))).is_ok() {
                        return Some(MemoryChange {
                            key: s.key.clone(),
                            operation: MemoryOperation::Archived,
                            scope: MemoryScope::Auto,
                        });
                    }
                }
            }
        }
        None
    }

    pub(super) fn key_exists_anywhere(&self, key: &str) -> bool {
        let filename = format!("{key}.md");
        self.roots
            .iter()
            .any(|r| r.join(&filename).is_file() || r.join(ARCHIVE_DIR).join(&filename).is_file())
    }
}
