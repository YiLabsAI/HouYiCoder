//! Session picker state: the inline overlay for /resume. Follows the
//! slash-palette shape (open + sel + query + filtered + prev/next +
//! push/pop) so the picker renders in the same inline cell the palette
//! uses, but over a dynamic session list. The list itself is loaded by the
//! CLI session catalog (SessionCatalog), which reads the descriptor store +
//! each session log head; the TUI stays a presentation layer and never names
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
    /// Log file size in bytes (log.jsonl), for a compact size column.
    pub log_size: u64,
    /// True when this row duplicates a newer row (same resolved title) and
    /// should not render. Set as resolve_detail fills real titles, in both
    /// the open-time batch and the poll loop; the render + the query filter
    /// skip hidden rows. Defaults false.
    pub hidden: bool,
}

/// The picker overlay state. The rows field is loaded once when the picker
/// opens (via the session catalog); the query narrows the loaded rows
/// client-side. Selection wraps the filtered list.
#[derive(Debug, Clone, Default)]
pub struct SessionPickerState {
    pub open: bool,
    pub sel: usize,
    pub query: String,
    pub rows: Vec<SessionRow>,
    pub resolved: std::collections::HashSet<usize>,
    /// Titles already claimed, in resolution order. When resolve_detail
    /// fills a row's real title and it matches a title in this set, the row
    /// is an older duplicate -> hidden. Empty at open: the first row to
    /// claim a title keeps it.
    pub seen_titles: std::collections::HashSet<String>,
}

/// The storage-facing trait the CLI implements: the resumable sessions (with
/// a derived title each) excluding the current one, plus the lazy detail
/// resolution. The TUI names this trait, the CLI provides it over the
/// descriptor store + the SessionLog, so the TUI never imports the storage
/// traits (dep-graph layering). Returns rows newest-updated first.
///
/// Two-phase progressive loading: sessions walks the store once and reads a
/// descriptor only where a row could take a visible slot, so the picker
/// opens after that one walk and never per frame; rows arrive sorted by real
/// last activity. resolve_detail fills in the expensive field (title from a
/// log-head read + serde parse) lazily for visible rows, a few per frame.
pub trait SessionCatalog: Send + Sync {
    fn sessions(&self, current_sid: &str) -> Vec<SessionRow>;
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
    }

    /// Resolve detail for up to budget rows that have none yet. Rows arrive
    /// newest-first and the first row to claim a title keeps it, so a row
    /// whose resolved title duplicates a newer row's is hidden; a descriptor
    /// name gives no precedence over an unnamed row resolved earlier. The
    /// claim set holds only rows already resolved, so a row never hides on
    /// its own title — a hide always means a collision with another row.
    pub fn resolve_rows(&mut self, catalog: &dyn SessionCatalog, budget: usize) {
        let mut done = 0;
        for i in 0..self.rows.len() {
            if done >= budget {
                break;
            }
            if self.resolved.contains(&i) {
                continue;
            }
            catalog.resolve_detail(&mut self.rows[i]);
            self.resolved.insert(i);
            done += 1;
            let title = self.rows[i].title.clone();
            if !self.seen_titles.insert(title) {
                self.rows[i].hidden = true;
            }
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

/// Compact log-size string: 12B, 4K, 1M. Right-aligned to 5 cols so the
/// column stays narrow and the title start position is stable.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{}K", bytes / 1024)
    } else {
        format!("{}M", bytes / (1024 * 1024))
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
