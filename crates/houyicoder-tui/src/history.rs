//! Prompt input history: submitted prompts stored in a JSONL file, recalled
//! with Up/Down. Newest first, project-filtered, current session's entries
//! first. Append-only; an aborted submit is soft-skipped (timestamp set read
//! consults) so the file is never rewritten. The file path is injected by the
//! composition root -- this crate does not depend on the config layer.

use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
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
    path: std::path::PathBuf,
}

impl HistoryNav {
    /// Set the history file path (injected by the composition root).
    pub fn with_path(path: std::path::PathBuf) -> Self {
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
    pub fn up(
        &mut self,
        input: &mut crate::input::InputField,
        project: &str,
        session: &str,
    ) -> bool {
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
    pub fn down(&mut self, input: &mut crate::input::InputField) -> bool {
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
mod tests {
    use super::*;
    use crate::input::InputField;

    fn nav() -> HistoryNav {
        HistoryNav::default()
    }

    fn field(s: &str) -> InputField {
        let mut f = InputField::new();
        f.set(s.to_string());
        f
    }

    #[test]
    fn test_up_recalls_older() {
        let mut n = nav();
        n.cache = vec!["newer".into(), "older".into()];
        n.loaded = true;
        let mut f = field("");
        assert!(n.up(&mut f, "p", "s"));
        assert_eq!(f.value(), "newer");
        assert_eq!(f.cursor(), 0);
        assert!(n.up(&mut f, "p", "s"));
        assert_eq!(f.value(), "older");
    }

    #[test]
    fn test_up_stops_at_oldest() {
        let mut n = nav();
        n.cache = vec!["only".into()];
        n.loaded = true;
        let mut f = field("");
        assert!(n.up(&mut f, "p", "s"));
        assert!(!n.up(&mut f, "p", "s"));
        assert_eq!(f.value(), "only");
    }

    #[test]
    fn test_down_restores_draft() {
        let mut n = nav();
        n.cache = vec!["old".into()];
        n.loaded = true;
        let mut f = field("my draft");
        assert!(n.up(&mut f, "p", "s"));
        assert_eq!(f.value(), "old");
        assert!(n.down(&mut f));
        assert_eq!(f.value(), "my draft");
        assert_eq!(f.cursor(), "my draft".len());
    }

    #[test]
    fn test_down_clears_no_draft() {
        let mut n = nav();
        n.cache = vec!["old".into()];
        n.loaded = true;
        let mut f = field("");
        assert!(n.up(&mut f, "p", "s"));
        assert!(n.down(&mut f));
        assert_eq!(f.value(), "");
    }

    #[test]
    fn test_down_noop_idle() {
        let mut n = nav();
        let mut f = field("text");
        assert!(!n.down(&mut f));
    }

    #[test]
    fn test_draft_skipped_empty() {
        let mut n = nav();
        n.cache = vec!["old".into()];
        n.loaded = true;
        let mut f = field("   ");
        assert!(n.up(&mut f, "p", "s"));
        assert!(n.down(&mut f));
        assert_eq!(f.value(), "");
    }

    #[test]
    fn test_remove_last_soft_skips() {
        let mut n = nav();
        n.last_added = Some(42);
        n.remove_last();
        assert!(n.skip.contains(&42));
        assert!(n.last_added.is_none());
        n.remove_last();
        assert_eq!(n.skip.len(), 1);
    }

    #[test]
    fn test_reset_clears_skip() {
        let mut n = nav();
        n.last_added = Some(1);
        n.skip.insert(1);
        n.cache = vec!["x".into()];
        n.index = 1;
        n.loaded = true;
        n.reset();
        assert!(n.skip.is_empty());
        assert!(n.last_added.is_none());
        assert_eq!(n.index, 0);
        assert!(!n.loaded);
    }

    #[test]
    fn test_add_read_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("houyi-hist-rt-{}-{}", std::process::id(), now_ts()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        add("hello world", "proj", "sess", &path);
        add("second", "proj", "sess", &path);
        let skip = HashSet::new();
        let got = read("proj", "sess", &skip, &path);
        assert_eq!(got, vec!["second".to_string(), "hello world".to_string()]);
        let got_other = read("other", "sess", &skip, &path);
        assert!(got_other.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_skip_set_drops_aborted() {
        let dir = std::env::temp_dir().join(format!(
            "houyi-hist-skip-{}-{}",
            std::process::id(),
            now_ts()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        let _keep = add("keep", "proj", "sess", &path);
        let abort = add("abort", "proj", "sess", &path);
        let mut skip = HashSet::new();
        skip.insert(abort);
        let got = read("proj", "sess", &skip, &path);
        assert_eq!(got, vec!["keep".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
