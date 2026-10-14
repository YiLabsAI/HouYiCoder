//! Adapt durable session descriptors and logs to the TUI SessionCatalog port.

use std::sync::Arc;

use houyicoder_api::session::SessionLog;
use houyicoder_context::session_class::{LOG_FILE, recent_user_sessions};
use houyicoder_context::{SessionDescriptorStore, SessionEvent, SessionId, SessionLogEntry};
use houyicoder_service::composition::disk_descriptor_store_at;
use houyicoder_tui::resume_picker::{SessionCatalog, SessionRow};
use std::collections::HashSet;
use std::fs::metadata;
use std::path::PathBuf;

pub struct DescriptorSessionCatalog {
    descriptor_store: Arc<dyn SessionDescriptorStore>,
    session_log: Arc<dyn SessionLog>,
    sessions_root: PathBuf,
}

impl DescriptorSessionCatalog {
    /// Build both session readers from the same root.
    pub fn new(session_log: Arc<dyn SessionLog>, sessions_root: PathBuf) -> Self {
        let descriptor_store = disk_descriptor_store_at(sessions_root.clone());
        Self {
            descriptor_store,
            session_log,
            sessions_root,
        }
    }
}

impl SessionCatalog for DescriptorSessionCatalog {
    fn sessions(&self, current_sid: &str) -> Vec<SessionRow> {
        let current = SessionId::from_display_string(current_sid).unwrap_or_default();
        // The listing already holds the user's own resumable sessions, ranked
        // by last-active, so this only applies the visible limit.
        const VISIBLE_LIMIT: usize = 100;
        let recent = recent_user_sessions(&self.sessions_root, VISIBLE_LIMIT);
        let mut rows: Vec<SessionRow> = recent
            .into_iter()
            .filter(|entry| entry.sid != current)
            .filter_map(|entry| {
                // A user session always has its descriptor, and the entry carries
                // both it and the directory it was scanned from, so a row costs
                // no second read.
                let descriptor = entry.descriptor?;
                let cwd_basename = descriptor
                    .cwd
                    .rsplit('/')
                    .next()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("?")
                    .to_string();
                let title = descriptor
                    .name
                    .as_ref()
                    .filter(|n| !n.trim().is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("(session) {}", short_sid(entry.sid)));
                let log_size = metadata(entry.path.join(LOG_FILE))
                    .map(|m| m.len())
                    .unwrap_or(0);
                Some(SessionRow {
                    sid_str: entry.sid.to_string(),
                    title,
                    cwd_basename,
                    last_active: entry.last_active,
                    log_size,
                    ..Default::default()
                })
            })
            .collect();
        // Already sorted by last_active desc by the listing, which also already
        // dropped every non-user session; only the current session is removed
        // above, and a removal preserves order -- so no re-sort is needed.
        // Dedup by the cheap title: when multiple sessions share the same
        // descriptor name (the common "re-running + naming alike" case), keep
        // only the most recently active one. The sort put the newest first,
        // so the first occurrence of each title wins. Placeholder titles are
        // unique (short sid suffix), so unnamed sessions never dedup here --
        // their slug-dedup happens lazily in the picker after resolve_detail
        // fills the real title (SessionPickerState::resolve_rows).
        let mut seen_titles: HashSet<String> = HashSet::new();
        rows.retain(|r| seen_titles.insert(r.title.clone()));
        rows
    }

    fn resolve_detail(&self, row: &mut SessionRow) {
        let Some(sid) = SessionId::from_display_string(&row.sid_str) else {
            return;
        };
        // Re-read the descriptor to decide the title unambiguously: a user-set
        // name wins (even if it happens to start with "(session)", which the
        // old starts_with heuristic would have mistaken for a placeholder).
        // Only when there is no descriptor name do we pay the log-head read +
        // serde parse for the first-prompt slug. last_active is already the
        // log mtime the listing carried, so no re-stat here.
        let has_name = self
            .descriptor_store
            .read_descriptor(sid)
            .as_ref()
            .and_then(|m| m.name.as_ref())
            .is_some_and(|n| !n.trim().is_empty());
        if has_name {
            return;
        }
        if let Some(prompt) = first_user_prompt(self.session_log.as_ref(), sid) {
            let slug = slugify(&prompt);
            // Guard: a prompt with no alphanumeric chars (e.g. "???", pure
            // whitespace) slugifies to empty -- keep the disambiguating
            // placeholder rather than show a blank row.
            if !slug.is_empty() {
                row.title = slug;
            }
        }
    }
}

/// The first 8 hex chars of a session id, for the disambiguating placeholder.
fn short_sid(sid: SessionId) -> String {
    sid.to_string().chars().take(8).collect()
}

fn first_user_prompt(session_log: &dyn SessionLog, sid: SessionId) -> Option<String> {
    let backend = session_log.backend();
    let read = backend.read_log_range(sid, 0, 64_000);
    for (_, line) in &read.lines {
        if let Ok(ev) = serde_json::from_str::<SessionLogEntry>(line)
            && let SessionEvent::UserInput { text } = &ev.event
        {
            return Some(text.clone());
        }
    }
    None
}

fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_dash = true;
    for c in text.chars().take(50) {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.chars().count() > 40 {
        let mut truncated: String = out.chars().take(39).collect();
        while truncated.ends_with('-') {
            truncated.pop();
        }
        truncated.push('\u{2026}');
        truncated
    } else {
        out
    }
}

#[cfg(test)]
mod tests;
