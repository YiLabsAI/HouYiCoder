//! Prompt input history: submitted prompts stored in a JSONL file, recalled
//! with Up/Down. Newest first, project-filtered, current session's entries
//! first. Append-only; an aborted submit is soft-skipped (timestamp set read
//! consults) so the file is never rewritten. The file path is injected by the
//! composition root -- this crate does not depend on the config layer.

use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::input::InputField;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

const MAX_ENTRIES: usize = 100;

fn project_root() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_default()
}

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize)]
struct Entry {
    display: String,
    timestamp: u64,
    project: String,
    session: String,
}

/// Append a submitted prompt to the given history file. No dedup, no trim --
/// raw text, newlines intact. 0o600 on Unix so the prompt history (may carry
/// secrets) is owner-only. Returns the entry's timestamp for abort-skip.
/// Best-effort: a write failure is logged and never blocks a submit.
pub fn add(display: &str, project: &str, session: &str, path: &Path) -> u64 {
    let ts = now_ts();
    let entry = Entry {
        display: display.to_string(),
        timestamp: ts,
        project: project.to_string(),
        session: session.to_string(),
    };
    let line = match serde_json::to_string(&entry) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "history serialize");
            return ts;
        }
    };
    if let Some(parent) = path.parent() {
        let _created = std::fs::create_dir_all(parent);
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(path) {
        let _written = writeln!(f, "{line}");
    }
    ts
}

/// Read the recall list from the given file: newest first, project-filtered,
/// current session first. Aborted-submit timestamps in the skip-set drop.
/// Capped at MAX_ENTRIES.
pub fn read(project: &str, session: &str, skip: &HashSet<u64>, path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut this_session: Vec<String> = Vec::new();
    let mut other_session: Vec<String> = Vec::new();
    for line in content.lines().rev() {
        let Ok(e) = serde_json::from_str::<Entry>(line) else {
            continue;
        };
        if e.project != project || skip.contains(&e.timestamp) {
            continue;
        }
        if e.session == session {
            this_session.push(e.display);
        } else {
            other_session.push(e.display);
        }
    }
    this_session.extend(other_session);
    this_session.truncate(MAX_ENTRIES);
    this_session
}

/// The current project root (the session cwd).
pub fn current_project() -> String {
    project_root()
}

/// Navigation cursor over the recall list. index 0 is the draft; 1..=cache.len
/// walks cache[0..] (newest first). last_added + skip track aborted submits so
/// read drops them without rewriting the file. Holds the history file path.
#[derive(Default)]
pub struct HistoryNav {
    cache: Vec<String>,
    index: usize,
    draft: Option<String>,
    loaded: bool,
    last_added: Option<u64>,
    skip: HashSet<u64>,
    path: PathBuf,
}

impl HistoryNav {
    /// Set the history file path (injected by the composition root).
    pub fn with_path(path: PathBuf) -> Self {
        HistoryNav {
            path,
            ..HistoryNav::default()
        }
    }

    /// Append a submitted prompt + reset the cursor so the next Up re-reads.
    pub fn submit(&mut self, display: &str, project: &str, session: &str) {
        let ts = add(display, project, session, &self.path);
        self.last_added = Some(ts);
        self.reset_nav();
    }

    /// Soft-skip the most recent submit (Esc-undo). One-shot.
    pub fn remove_last(&mut self) {
        if let Some(ts) = self.last_added.take() {
            self.skip.insert(ts);
        }
    }

    /// Recall one step older. Cursor lands at the start. Stops at the oldest.
    pub fn up(&mut self, input: &mut InputField, project: &str, session: &str) -> bool {
        if !self.loaded {
            self.cache = read(project, session, &self.skip, &self.path);
            self.loaded = true;
        }
        if self.cache.is_empty() {
            return false;
        }
        if self.index == 0 && !input.value().trim().is_empty() {
            self.draft = Some(input.value().to_string());
        }
        if self.index >= self.cache.len() {
            return false;
        }
        self.index += 1;
        input.set_at_start(self.cache[self.index - 1].clone());
        true
    }

    /// Recall one step newer, or restore the draft past the newest. Cursor at end.
    pub fn down(&mut self, input: &mut InputField) -> bool {
        if self.index == 0 {
            return false;
        }
        self.index -= 1;
        if self.index == 0 {
            match self.draft.take() {
                Some(d) => input.set(d),
                None => input.clear(),
            }
            return true;
        }
        input.set(self.cache[self.index - 1].clone());
        true
    }

    fn reset_nav(&mut self) {
        self.cache.clear();
        self.index = 0;
        self.draft = None;
        self.loaded = false;
    }

    /// Full reset (/clear or session swap): drops nav + skip + last_added.
    pub fn reset(&mut self) {
        self.reset_nav();
        self.last_added = None;
        self.skip.clear();
    }
}

#[cfg(test)]
mod tests;
