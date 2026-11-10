//! Session picker state: the inline overlay for /resume. Follows the
//! slash-palette shape (open + sel + query + filtered + prev/next +
//! push/pop) so the picker renders in the same inline cell the palette
//! uses, but over a dynamic session list. The list itself is loaded by the
//! CLI session catalog (SessionCatalog) in one pass when the picker opens;
//! the TUI stays a presentation layer and never names the storage traits
//! directly (the dep-graph layering).

/// Where a row's title came from. The picker reads it to decide whether a
/// secondary line adds anything: a name the user chose says nothing about
/// the work, so the line then carries the newest prompt, while a title that
/// already is the newest prompt leaves the opening subject for the line.
/// The catalog derives it from the descriptor's name source; the picker only
/// needs the two distinctions the line depends on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TitleSource {
    /// Neither a name nor a prompt: the row shows its short session id.
    #[default]
    Placeholder,
    /// A name set with /rename.
    User,
    /// The newest prompt that states the work.
    LastPrompt,
    /// The first prompt that states the work.
    FirstPrompt,
}

/// One row in the picker. The sid is NOT shown (the design density
/// decision: the user knows sessions by name + cwd, the sid is queryable
/// via /status). The relative time is a compact form so the row fits one
/// line alongside the title + the cwd basename.
#[derive(Debug, Clone, Default)]
pub struct SessionRow {
    pub sid_str: String,
    pub title: String,
    /// Which heading the title is, as far as the secondary line is
    /// concerned. The catalog sets it when it derives the title.
    pub title_source: TitleSource,
    pub cwd_basename: String,
    /// The first and newest prompts the session's record carries. They serve
    /// the filter and the selected row's topic line; None when the record
    /// carries none, which is what a descriptor written before prompts were
    /// recorded looks like.
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    /// Unix-epoch seconds of the session last update.
    pub last_active: u64,
    /// Log file size in bytes (log.jsonl), for a compact size column.
    pub log_size: u64,
    /// The session's lock was held by another process or could not be read
    /// when the catalog read the listing, so this process cannot open it now.
    /// The row stays listed so an explicit lookup by name finds the session
    /// and its switch reaches the refusal that names the blocker; the picker
    /// skips it.
    pub unavailable: bool,
}

/// The most rows the picker keeps after skipping sessions this process
/// cannot open now. The catalog reads a little past this limit so an
/// unavailable row cannot cost a free session its place.
pub const VISIBLE_LIMIT: usize = 100;

impl SessionRow {
    /// Whether a filter query hits this row: the title, either prompt, the
    /// project directory name, or the session id, compared
    /// case-insensitively. An empty query matches every row.
    pub fn matches(&self, query: &str) -> bool {
        self.matches_needle(&query.trim().to_lowercase())
    }

    /// The same test against a needle that is already trimmed and lowercased,
    /// so a scan over the listing normalizes the typed query once.
    fn matches_needle(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let hit = |text: &str| contains_folded(text, needle);
        hit(&self.sid_str)
            || hit(&self.title)
            || hit(&self.cwd_basename)
            || self.first_prompt.as_deref().is_some_and(hit)
            || self.last_prompt.as_deref().is_some_and(hit)
    }

    /// The secondary line under the selected row, or None when it would only
    /// repeat the title. A user-chosen name says nothing about the work, so
    /// the line carries the newest prompt; a title already taken from the
    /// newest prompt leaves the opening subject. This states what the user
    /// last asked for, not what was finished.
    pub fn topic_line(&self) -> Option<String> {
        let topic = match self.title_source {
            TitleSource::User => self.last_prompt.as_deref(),
            TitleSource::LastPrompt => self.first_prompt.as_deref(),
            TitleSource::FirstPrompt | TitleSource::Placeholder => None,
        }?
        .trim();
        (!topic.is_empty() && topic != self.title).then(|| format!("topic: {topic}"))
    }
}

/// Whether text contains needle, ignoring case. The needle arrives already
/// folded. ASCII text folds per byte without allocating, which is the common
/// path; text in another script is folded in full so a query typed in that
/// script still matches.
fn contains_folded(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if text.is_ascii() && needle.is_ascii() {
        let pattern = needle.as_bytes();
        return text
            .as_bytes()
            .windows(pattern.len())
            .any(|window| window.eq_ignore_ascii_case(pattern));
    }
    text.to_lowercase().contains(needle)
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
    /// How many sessions the catalog left out of rows because this process
    /// cannot open them now. The pane header states the count so the absence
    /// is visible where the listing is.
    pub hidden: usize,
}

/// The storage-facing trait the CLI implements: the resumable sessions (with
/// a derived title each) excluding the current one. The TUI names this
/// trait, the CLI provides it over the descriptor store, so the TUI never
/// imports the storage traits (dep-graph layering). Returns rows
/// newest-updated first.
///
/// The walk reads a descriptor only where a row could take a visible slot,
/// so the picker opens after one store walk and never reads a session log.
/// A session whose record states no prompt is titled by a bounded migration
/// pass that fills those records in the background, not by this call.
pub trait SessionCatalog: Send + Sync {
    fn sessions(&self, current_sid: &str) -> Vec<SessionRow>;
}

impl SessionPickerState {
    /// The filtered list: a row matches when the query hits its title, either
    /// prompt, the project directory name, or the sid. Empty query returns
    /// every row.
    pub fn filtered(&self) -> Vec<&SessionRow> {
        let needle = self.query.trim().to_lowercase();
        self.rows
            .iter()
            .filter(|row| row.matches_needle(&needle))
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
    }

    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.sel = 0;
    }
}

/// A row's age as the picker prints it: now, 5m, 2h, 3d, 2y. Bounded to four
/// columns so the title starts at the same column on every row. Distinct from
/// the shared relative-time stamp, which reads "5m ago" and takes its
/// arguments the other way around.
pub fn age_column(last_active: u64, now_secs: u64) -> String {
    let delta = now_secs.saturating_sub(last_active);
    if delta < 60 {
        "now".to_string()
    } else if delta < 3600 {
        format!("{}m", delta / 60)
    } else if delta < 86_400 {
        format!("{}h", delta / 3600)
    } else if delta < 86_400 * 365 {
        format!("{}d", delta / 86_400)
    } else {
        format!("{}y", (delta / (86_400 * 365)).min(99))
    }
}

/// Compact log-size string: 12B, 4K, 1M, 2G, 3T. Bounded to five columns so
/// the title start position is stable across rows, which caps the largest
/// tier at two digits.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{}K", bytes / 1024)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{}M", bytes / (1024 * 1024))
    } else if bytes < 1024 * 1024 * 1024 * 1024 {
        format!("{}G", bytes / (1024 * 1024 * 1024))
    } else {
        format!("{}T", (bytes / (1024 * 1024 * 1024 * 1024)).min(99))
    }
}

#[cfg(test)]
mod tests;
