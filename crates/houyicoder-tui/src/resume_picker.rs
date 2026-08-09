//! Session picker state: the inline overlay for /resume. Follows the
//! slash-palette shape (open + sel + query + filtered + prev/next +
//! push/pop) so the picker renders in the same inline cell the palette
//! uses, but over a dynamic session list. The list itself is loaded by the
//! CLI bridge (SessionLister), which reads the sidecar store + each
//! session log head; the TUI stays a presentation layer and never names
//! the storage traits directly (the dep-graph layering).

/// One row in the picker. The sid is NOT shown (the design density
/// decision: the user knows sessions by name + cwd, the sid is queryable
/// via /status). The relative time is a compact form so the row fits one
/// line alongside the title + the cwd basename.
#[derive(Debug, Clone, Default)]
pub struct SessionRow {
    pub sid_str: String,
    pub title: String,
    pub cwd_basename: String,
    /// Unix-epoch seconds of the session last update.
    pub last_active: u64,
    /// True when this row is a duplicate of a newer row (same resolved
    /// title) and should not render. Set lazily by the poll-loop dedup
    /// pass as resolve_detail fills real titles; the render + the query
    /// filter skip hidden rows. Defaults false.
    pub hidden: bool,
}

/// The picker overlay state. The rows field is loaded once when the picker
/// opens (via the SessionLister bridge); the query narrows the loaded rows
/// client-side. Selection wraps the filtered list.
#[derive(Debug, Clone, Default)]
pub struct SessionPickerState {
    pub open: bool,
    pub sel: usize,
    pub query: String,
    pub rows: Vec<SessionRow>,
    pub resolved: std::collections::HashSet<usize>,
    /// Titles already shown (newest-first resolution order). When
    /// resolve_detail fills a row's real title and it matches a title in
    /// this set, the row is an older duplicate -> hidden. Seeded at open
    /// from the cheap titles (sidecar names + unique placeholders) so a
    /// named session also suppresses same-slug unnamed ones.
    pub seen_titles: std::collections::HashSet<String>,
}

/// The storage-facing trait the CLI bridge implements: list the resumable
/// sessions (with a derived title each) excluding the current one. The TUI
/// names this trait, the CLI provides it over the sidecar store + the
/// SessionLog, so the TUI never imports the storage traits (dep-graph
/// layering). Returns rows newest-updated first.
///
/// Two-phase progressive loading: list_sessions is cheap (sidecar read +
/// one log mtime stat per session — no log-head read/parse) so the picker
/// opens instantly even with hundreds of sessions, sorted by real last
/// activity. resolve_detail fills in the expensive field (title from a
/// log-head read + serde parse) lazily for visible rows, a few per frame.
pub trait SessionLister: Send + Sync {
    fn list_sessions(&self, current_sid: &str) -> Vec<SessionRow>;
    fn resolve_detail(&self, row: &mut SessionRow);
}

impl SessionPickerState {
    /// The filtered list: a row matches when the inline query is a
    /// case-insensitive substring of the row sid OR its title (either
    /// matches -- the design OR, not AND). Empty query returns all rows.
    /// Hidden rows (older duplicates) are always excluded.
    pub fn filtered(&self) -> Vec<&SessionRow> {
        let q = self.query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return self.rows.iter().filter(|r| !r.hidden).collect();
        }
        self.rows
            .iter()
            .filter(|r| {
                !r.hidden
                    && (r.sid_str.to_ascii_lowercase().contains(&q)
                        || r.title.to_ascii_lowercase().contains(&q))
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.filtered().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The currently selected row, if the picker is open + the filtered
    /// list is non-empty.
    pub fn selected(&self) -> Option<&SessionRow> {
        if !self.open {
            return None;
        }
        let f = self.filtered();
        f.get(self.sel).copied()
    }

    pub fn prev(&mut self) {
        let n = self.filtered().len();
        if n > 0 {
            self.sel = (self.sel + n - 1) % n;
        }
    }

    pub fn next(&mut self) {
        let n = self.filtered().len();
        if n > 0 {
            self.sel = (self.sel + 1) % n;
        }
    }

    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.sel = 0;
    }

    pub fn pop(&mut self) {
        self.query.pop();
        self.sel = 0;
    }

    pub fn open(&mut self) {
        self.open = true;
        self.query.clear();
        self.sel = 0;
        self.resolved.clear();
        self.seen_titles.clear();
        // Seed the dedup set from the cheap titles already on the rows
        // (sidecar names + unique placeholders). list_sessions already
        // deduped by these, so each is unique here; seeding lets the lazy
        // slug-dedup suppress unnamed rows whose resolved slug collides
        // with a named session too.
        for r in &self.rows {
            self.seen_titles.insert(r.title.clone());
        }
    }

    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.sel = 0;
        self.resolved.clear();
        self.seen_titles.clear();
    }
}

/// A compact relative-time string for a row: now, 5m, 2h, 3d. Bounded so
/// the column stays narrow.
pub fn relative_time(last_active: u64, now_secs: u64) -> String {
    let delta = now_secs.saturating_sub(last_active);
    if delta < 60 {
        "now".to_string()
    } else if delta < 3600 {
        format!("{}m", delta / 60)
    } else if delta < 86_400 {
        format!("{}h", delta / 3600)
    } else {
        format!("{}d", delta / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sid: &str, title: &str, ts: u64) -> SessionRow {
        SessionRow {
            sid_str: sid.into(),
            title: title.into(),
            cwd_basename: "repo".into(),
            last_active: ts,
            ..Default::default()
        }
    }

    impl SessionPickerState {
        fn query_filter(&mut self, q: &str) -> Vec<&SessionRow> {
            self.query = q.into();
            self.sel = 0;
            self.filtered()
        }
    }

    #[test]
    fn test_empty_query_returns_all() {
        let p = SessionPickerState {
            rows: vec![row("a", "alpha", 1), row("b", "beta", 2)],
            ..Default::default()
        };
        assert_eq!(p.filtered().len(), 2);
    }

    #[test]
    fn test_query_matches_sid_title() {
        let mut p = SessionPickerState {
            rows: vec![
                row("11111111-1111-1111-1111-111111111111", "alpha", 1),
                row("22222222-2222-2222-2222-222222222222", "beta login", 2),
            ],
            ..Default::default()
        };
        assert_eq!(p.query_filter("1111").len(), 1);
        assert_eq!(p.query_filter("log").len(), 1);
        assert_eq!(p.query_filter("zzz").len(), 0);
    }

    #[test]
    fn test_selection_wraps() {
        let mut p = SessionPickerState {
            rows: vec![row("a", "x", 1), row("b", "y", 2)],
            ..Default::default()
        };
        p.open();
        assert_eq!(p.sel, 0);
        p.next();
        assert_eq!(p.sel, 1);
        p.next();
        assert_eq!(p.sel, 0);
        p.prev();
        assert_eq!(p.sel, 1);
    }

    #[test]
    fn test_push_resets_selection() {
        let mut p = SessionPickerState {
            rows: vec![row("a", "x", 1), row("b", "y", 2)],
            ..Default::default()
        };
        p.open();
        p.next();
        assert_eq!(p.sel, 1);
        p.push('a');
        assert_eq!(p.sel, 0);
    }

    #[test]
    fn test_relative_time_buckets() {
        assert_eq!(relative_time(0, 0), "now");
        assert_eq!(relative_time(0, 30), "now");
        assert_eq!(relative_time(0, 120), "2m");
        assert_eq!(relative_time(0, 7200), "2h");
        assert_eq!(relative_time(0, 3 * 86_400), "3d");
    }
}
