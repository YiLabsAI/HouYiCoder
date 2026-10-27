//! Adapt durable session descriptors to the TUI SessionCatalog port.

use houyicoder_context::SessionId;
use houyicoder_context::session_class::{LOG_FILE, recent_user_sessions};
use houyicoder_context::session_intent::TitleSource as RecordTitleSource;
use houyicoder_context::session_intent::display_title;
use houyicoder_tui::resume_picker::{SessionCatalog, SessionRow, TitleSource};
use std::fs::metadata;
use std::path::Path;
use std::path::PathBuf;

/// The last segment of a project directory, for the row's narrow first
/// column. The lookup follows the path rules of the platform running the
/// listing rather than a literal separator.
fn cwd_basename(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "?".to_string())
}

pub struct DescriptorSessionCatalog {
    sessions_root: PathBuf,
}

impl DescriptorSessionCatalog {
    /// Build the catalog over one sessions root. Every row is answered from
    /// the store's directory listing and the session's record; no session
    /// log is opened, so opening the picker costs no log read per row.
    pub fn new(sessions_root: PathBuf) -> Self {
        Self { sessions_root }
    }
}

impl SessionCatalog for DescriptorSessionCatalog {
    fn sessions(&self, current_sid: &str) -> Vec<SessionRow> {
        let current = SessionId::from_display_string(current_sid).unwrap_or_default();
        // The listing already holds the user's own resumable sessions, ranked
        // by last-active, so this only applies the visible limit.
        const VISIBLE_LIMIT: usize = 100;
        let recent = recent_user_sessions(&self.sessions_root, VISIBLE_LIMIT);
        recent
            .into_iter()
            .filter(|entry| entry.sid != current)
            .filter_map(|entry| {
                // A user session always has its descriptor, and the entry carries
                // both it and the directory it was scanned from, so a row costs
                // no second read.
                let descriptor = entry.descriptor?;
                let cwd_basename = cwd_basename(&descriptor.cwd);
                let (title, title_source) = match display_title(&descriptor) {
                    Some(title) => (title.text.to_string(), row_source(title.source)),
                    None => (
                        format!("(session) {}", short_sid(entry.sid)),
                        TitleSource::Placeholder,
                    ),
                };
                let prompt =
                    |text: Option<&str>| text.map(str::to_string).filter(|t| !t.trim().is_empty());
                Some(SessionRow {
                    sid_str: entry.sid.to_string(),
                    title,
                    title_source,
                    cwd_basename,
                    first_prompt: prompt(descriptor.first_prompt.as_ref().map(|r| r.text.as_str())),
                    last_prompt: prompt(descriptor.last_prompt.as_ref().map(|r| r.text.as_str())),
                    last_active: entry.last_active,
                    log_size: metadata(entry.path.join(LOG_FILE))
                        .map(|m| m.len())
                        .unwrap_or(0),
                })
            })
            .collect()
    }
}

/// The picker's view of where a title came from, mapped from the record's
/// own account of it.
fn row_source(source: RecordTitleSource) -> TitleSource {
    match source {
        RecordTitleSource::User => TitleSource::User,
        RecordTitleSource::LastPrompt => TitleSource::LastPrompt,
        RecordTitleSource::FirstPrompt => TitleSource::FirstPrompt,
    }
}

/// The first 8 hex chars of a session id, for the disambiguating placeholder.
fn short_sid(sid: SessionId) -> String {
    sid.to_string().chars().take(8).collect()
}

#[cfg(test)]
mod tests;
