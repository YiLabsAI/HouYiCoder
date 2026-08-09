//! Transcript scrollback and search state. The transcript pane follows the
//! tail by default; PgUp breaks follow so older rows stay visible, and End
//! or a new user submission returns to the tail. Search has two modes: an
//! inline Ctrl+F bar that highlights matches in the transcript, and a /search
//! popup that lists matching lines and jumps the scroll to the focused one.
//!
//! Layout metrics the key handler needs (cap, total rows) are published by the
//! pure view through Cell so the view stays a read-only renderer of App while
//! paging still knows the real viewport size.

use std::cell::Cell;

use crate::input::InputField;
use crate::records::TranscriptLine;

/// Cap on the in-memory viewable transcript. An unbounded buffer is the
/// perf cliff: the per-frame display-row count walks every line, and search
/// scans every line on each run, so a long session makes every frame O(total
/// history). The raw transcript stays on the session log for full-history
/// recall; this is the bounded viewable projection, mirroring a native
/// terminal's bounded scrollback. Fold state is keyed by string (not line
/// index) so eviction does not corrupt it.
const VIEWABLE_SCROLLBACK_CAP: usize = 4000;

/// The search snapshot loads the whole log when at or under this size; over
/// it, the view opens in byte-window mode (one screen at a time) rather than
/// blowing memory.
/// 16 MB: p99 session log is ~2 MB, so this covers 99.3% of real sessions;
/// the spike measured the TranscriptLine footprint at ~1x raw (not the
/// guessed 2-4x), so 16 MB log is ~16 MB resident -- well under the budget.
pub const SEARCH_LOG_THRESHOLD: u64 = 16 * 1024 * 1024;

/// The byte budget for one windowed screen read. ~256 KB is ~70 events at
/// the measured 3.6 KB/event average -- enough to fill a screen with room
/// to scroll within the window before a new seek, small enough that peak
/// resident stays bounded regardless of log size.
pub const WINDOW_MAX_BYTES: u64 = 256 * 1024;

/// Bound the viewable scrollback. Evict the oldest lines once the in-memory
/// transcript exceeds the cap. The search view renders a frozen snapshot
/// (active_transcript), not this live vec, so an open search needs no
/// recompute on eviction -- its matches index into the snapshot which does
/// not shift. (The prior recompute was the index-shift fix the snapshot
/// makes dead; keeping it would recompute against the wrong vec and
/// overwrite the snapshot's matches.)
pub(crate) fn bound_scrollback(transcript: &mut Vec<TranscriptLine>) {
    if transcript.len() > VIEWABLE_SCROLLBACK_CAP {
        let drop = transcript.len() - VIEWABLE_SCROLLBACK_CAP;
        transcript.drain(0..drop);
    }
}

/// Scrollback state for the transcript pane. follow_tail true means the most
/// recent rows stay visible; false means a fixed top offset is pinned so older
/// content stays on screen while the user scrolls back.
#[derive(Debug, Clone)]
pub struct TranscriptScroll {
    /// True when pinned to the tail (new rows push in). False once PgUp or a
    /// wheel-up pins a top offset. End or a new user submission restores
    /// follow; mid-run agent output never yanks a reader scrolled back.
    pub follow_tail: bool,
    /// Top display-row index when not following the tail. Ignored while
    /// follow_tail is true.
    pub offset: usize,
    /// Viewport capacity in display rows, written by the view during draw so
    /// the key handler can page by a full screen. Defaults to a sane value so
    /// paging works before the first draw.
    pub cap: Cell<usize>,
    /// Total display-row count, written by the view during draw so the key
    /// handler can clamp and detect end-of-buffer.
    pub total: Cell<usize>,
}

impl Default for TranscriptScroll {
    fn default() -> Self {
        Self {
            follow_tail: true,
            offset: 0,
            cap: Cell::new(0),
            total: Cell::new(0),
        }
    }
}

impl TranscriptScroll {
    pub fn raw_top(&self) -> usize {
        self.offset
    }

    pub fn set_raw_top(&mut self, v: usize) {
        self.offset = v;
    }

    /// Effective cap (rows visible). Falls back to 1 when the view has not
    /// published a value yet so paging never divides by zero.
    pub fn effective_cap(&self) -> usize {
        let c = self.cap.get();
        if c == 0 { 1 } else { c }
    }

    /// The top display-row index to render from. Follows the tail when
    /// follow_tail is true; otherwise the pinned offset clamped to the valid
    /// range so the viewport always fills: never starts past total - cap.
    pub fn top_offset(&self, total_rows: usize) -> usize {
        let cap = self.effective_cap();
        if self.follow_tail {
            total_rows.saturating_sub(cap)
        } else {
            self.offset.min(total_rows.saturating_sub(cap))
        }
    }

    /// Number of rows hidden above the current viewport (the N in N more).
    pub fn more_above(&self, total_rows: usize) -> usize {
        self.top_offset(total_rows)
    }

    /// Page up by one viewport. Breaks follow-tail and pins the offset. A
    /// no-op (keeping follow-tail) when the transcript fits one viewport or
    /// less — otherwise a wheel on a short session would pin offset 0 and
    /// surface a ghost "jump to bottom" pill while the view is already at
    /// the bottom.
    pub fn page_up(&mut self, total_rows: usize) {
        let cap = self.effective_cap();
        let max_top = total_rows.saturating_sub(cap);
        if max_top == 0 {
            return;
        }
        let cur = self.top_offset(total_rows);
        self.offset = cur.saturating_sub(cap);
        self.follow_tail = false;
    }

    /// Page down by one viewport. Returns to follow-tail when the bottom is
    /// reached.
    pub fn page_down(&mut self, total_rows: usize) {
        let cap = self.effective_cap();
        let cur = self.top_offset(total_rows);
        let max_top = total_rows.saturating_sub(cap);
        if cur + cap >= max_top {
            self.follow_tail = true;
        } else {
            self.offset = cur + cap;
            self.follow_tail = false;
        }
    }

    /// Step up by n lines. Breaks follow-tail and pins the offset. Used for
    /// wheel scroll (n = 3) and edge auto-scroll during a drag (n = 1) — a
    /// line step keeps continuity with the prior viewport, unlike a full
    /// page jump which swaps the whole screen. A no-op (keeping follow-tail)
    /// when the transcript fits one viewport or less, matching page_up.
    pub fn line_up(&mut self, n: usize, total_rows: usize) {
        let max_top = total_rows.saturating_sub(self.effective_cap());
        if max_top == 0 {
            return;
        }
        let cur = self.top_offset(total_rows);
        self.offset = cur.saturating_sub(n);
        self.follow_tail = false;
    }

    /// Step down by n lines. Returns to follow-tail when the bottom is
    /// reached. See line_up for why a line step is the wheel default.
    pub fn line_down(&mut self, n: usize, total_rows: usize) {
        let cur = self.top_offset(total_rows);
        let max_top = total_rows.saturating_sub(self.effective_cap());
        if cur + n >= max_top {
            self.follow_tail = true;
        } else {
            self.offset = cur + n;
            self.follow_tail = false;
        }
    }

    /// Pin the viewport so the given display-row index is at the top.
    pub fn jump_to(&mut self, row: usize) {
        self.offset = row;
        self.follow_tail = false;
    }

    /// Return to following the tail.
    pub fn follow_tail(&mut self) {
        self.follow_tail = true;
    }
}

/// Within-window row scroll for the byte-window search view. Separate from
/// TranscriptScroll so window mode does not touch the whole-vec scroll state
/// or its consumers: the slot layer (fold grouping + collapse handles) has no
/// meaning when only one screen is materialized at a time, so the window
/// view renders flat through its own path and keeps its own row offset. The
/// row-offset math itself (follow-tail, page/line step, clamp to total-cap)
/// is the same on the window's rendered rows.
#[derive(Debug, Clone, Default)]
pub struct WindowScroll {
    /// True when pinned to the window's tail (newest rows). False once the
    /// user pages up within the window.
    pub follow_tail: bool,
    /// Top display-row offset within the window when not following the tail.
    pub offset: usize,
    /// Viewport capacity in rows, written by the flat draw path.
    pub cap: Cell<usize>,
    /// Window row count, written by the flat draw path.
    pub total: Cell<usize>,
}

impl WindowScroll {
    /// Effective cap (rows visible). Falls back to 1 before the first draw.
    pub fn effective_cap(&self) -> usize {
        let c = self.cap.get();
        if c == 0 { 1 } else { c }
    }

    /// The top row index to render from. Follows the tail when follow_tail is
    /// true; otherwise the pinned offset clamped so the viewport always fills.
    pub fn top_offset(&self) -> usize {
        let total = self.total.get();
        let cap = self.effective_cap();
        if self.follow_tail {
            total.saturating_sub(cap)
        } else {
            self.offset.min(total.saturating_sub(cap))
        }
    }

    /// Page up by one viewport within the window. No-op when the window fits
    /// one viewport or less.
    pub fn page_up(&mut self) {
        let total = self.total.get();
        let cap = self.effective_cap();
        let max_top = total.saturating_sub(cap);
        if max_top == 0 {
            return;
        }
        let cur = self.top_offset();
        self.offset = cur.saturating_sub(cap);
        self.follow_tail = false;
    }

    /// Page down by one viewport within the window. Returns to follow-tail
    /// when the window bottom is reached.
    pub fn page_down(&mut self) {
        let total = self.total.get();
        let cap = self.effective_cap();
        let cur = self.top_offset();
        let max_top = total.saturating_sub(cap);
        if cur + cap >= max_top {
            self.follow_tail = true;
        } else {
            self.offset = cur + cap;
            self.follow_tail = false;
        }
    }

    /// Step up by n rows within the window (wheel scroll).
    pub fn line_up(&mut self, n: usize) {
        let total = self.total.get();
        let max_top = total.saturating_sub(self.effective_cap());
        if max_top == 0 {
            return;
        }
        let cur = self.top_offset();
        self.offset = cur.saturating_sub(n);
        self.follow_tail = false;
    }

    /// Step down by n rows within the window. Returns to follow-tail at the
    /// bottom.
    pub fn line_down(&mut self, n: usize) {
        let total = self.total.get();
        let cur = self.top_offset();
        let max_top = total.saturating_sub(self.effective_cap());
        if cur + n >= max_top {
            self.follow_tail = true;
        } else {
            self.offset = cur + n;
            self.follow_tail = false;
        }
    }

    /// Pin the viewport so the given row is at the top.
    pub fn jump_to(&mut self, row: usize) {
        self.offset = row;
        self.follow_tail = false;
    }

    /// Return to following the window tail.
    pub fn follow_tail(&mut self) {
        self.follow_tail = true;
    }
}

/// Search state for the /search view. matches holds transcript line indices
/// (not display rows); the view maps them to rows when rendering highlights.
/// The legacy inline/popup/detail/disk surfaces are gone (decision 5: only
/// /search, as a snapshot verbose view). The active flag is the single source
/// of "search view open": enter/exit set it, the highlight gate reads it, and
/// bound_scrollback reads it to recompute matches after evicting old lines so
/// the indices stay valid mid-search.
#[derive(Debug, Default, Clone)]
pub struct SearchState {
    /// True while the search view is active. Single source: read by the
    /// highlight gate, the chrome, and bound_scrollback (which recomputes
    /// matches when the transcript is evicted past the 4000-row cap, so a
    /// long-running agent does not leave stale indices under an open search).
    pub active: bool,
    /// The current query string.
    pub query: String,
    /// Transcript line indices whose rendered text contains the query.
    pub matches: Vec<usize>,
    /// Index into matches of the currently focused result.
    pub focus: usize,
    /// True while the in-view slash re-search input bar is open. When true,
    /// printable and edit keys route to the input field; Enter commits
    /// (overwrites query, re-runs, re-focuses the newest match) and Esc
    /// cancels (discards the buffer; query/matches/focus are untouched
    /// until commit, so cancel is a trivial no-op restore). Snapshot
    /// semantics (decision 5): the bar is not per-keystroke-live like a
    /// transcript search bar; the re-scan runs once on Enter.
    pub input_mode: bool,
    /// The in-view slash bar buffer. Seeded with the current query on entry
    /// (less-style: slash shows the last pattern, editable). See input_mode.
    pub input: InputField,
}

impl SearchState {
    /// Compute matches for the current query over the transcript. Case is
    /// ignored. Resets focus to the first match. Slash-command User echoes
    /// (lines starting with /) are skipped so /search X does not self-match
    /// its own command echo.
    pub fn run(&mut self, transcript: &[TranscriptLine]) {
        self.matches = transcript
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                let q = self.query.trim().to_ascii_lowercase();
                if q.is_empty() {
                    return false;
                }
                // Skip slash-command echoes — a /search X line would otherwise
                // match its own query token, and other /command echoes are
                // invocations, not content to search.
                if let TranscriptLine::User(s) = l
                    && s.trim_start().starts_with('/')
                {
                    return false;
                }
                l.search_text().to_ascii_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        self.focus = 0;
    }

    /// The transcript line index of the focused match, if any.
    pub fn focused_line(&self) -> Option<usize> {
        self.matches.get(self.focus).copied()
    }

    /// Advance to the next match, wrapping around.
    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.focus = (self.focus + 1) % self.matches.len();
        }
    }

    /// Step to the previous match, wrapping around.
    pub fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.focus = (self.focus + self.matches.len() - 1) % self.matches.len();
        }
    }

    /// Close the search surface.
    pub fn close(&mut self) {
        self.active = false;
        self.input_mode = false;
        self.query.clear();
        self.matches.clear();
        self.focus = 0;
        self.input.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(s: &str) -> TranscriptLine {
        TranscriptLine::System(s.to_string())
    }

    #[test]
    fn test_page_up_breaks_follow() {
        let mut s = TranscriptScroll::default();
        s.cap.set(5);
        s.total.set(20);
        assert!(s.follow_tail);
        assert_eq!(s.top_offset(20), 15);
        s.page_up(20);
        assert!(!s.follow_tail);
        assert_eq!(s.top_offset(20), 10);
    }

    #[test]
    fn test_page_down_reaches_tail() {
        let mut s = TranscriptScroll::default();
        s.cap.set(5);
        s.total.set(20);
        s.page_up(20);
        assert!(!s.follow_tail);
        // page down once -> offset 15, still not at max_top(15) edge
        s.page_down(20);
        // one more page down reaches the bottom -> follow tail
        s.page_down(20);
        assert!(s.follow_tail);
    }

    #[test]
    fn test_jump_to_pins_offset() {
        let mut s = TranscriptScroll::default();
        s.cap.set(5);
        s.jump_to(3);
        assert!(!s.follow_tail);
        assert_eq!(s.top_offset(20), 3);
    }

    #[test]
    fn test_line_down_three() {
        let mut s = TranscriptScroll::default();
        s.cap.set(5);
        // Pin near the middle first so a line step is visible vs a page jump.
        s.jump_to(5);
        assert_eq!(s.top_offset(20), 5);
        s.line_down(3, 20);
        assert_eq!(s.top_offset(20), 8, "line step is 3, not a full page of 5");
        assert!(!s.follow_tail);
    }

    #[test]
    fn test_line_up_three() {
        let mut s = TranscriptScroll::default();
        s.cap.set(5);
        s.jump_to(8);
        s.line_up(3, 20);
        assert_eq!(s.top_offset(20), 5);
        assert!(!s.follow_tail);
    }

    #[test]
    fn test_line_down_tail() {
        let mut s = TranscriptScroll::default();
        s.cap.set(5);
        s.jump_to(14);
        // max_top = 20 - 5 = 15; stepping 3 from 14 reaches the tail.
        s.line_down(3, 20);
        assert!(s.follow_tail, "reaching the bottom returns to follow-tail");
    }

    #[test]
    fn test_line_up_clamp() {
        let mut s = TranscriptScroll::default();
        s.cap.set(5);
        s.jump_to(1);
        s.line_up(3, 20);
        assert_eq!(s.top_offset(20), 0, "never underflows past the top");
    }

    #[test]
    fn test_more_above_counts_hidden() {
        let mut s = TranscriptScroll::default();
        s.cap.set(5);
        s.page_up(20);
        assert_eq!(s.more_above(20), 10);
    }

    #[test]
    fn test_search_finds_case_insensitive() {
        let mut s = SearchState {
            query: "BUG".to_string(),
            ..Default::default()
        };
        let t = vec![line("the bug is fixed"), line("nothing here")];
        s.run(&t);
        assert_eq!(s.matches, vec![0]);
        assert_eq!(s.focused_line(), Some(0));
    }

    #[test]
    fn test_search_next_wraps() {
        let mut s = SearchState {
            query: "x".to_string(),
            ..Default::default()
        };
        let t = vec![line("x one"), line("x two"), line("none")];
        s.run(&t);
        assert_eq!(s.matches, vec![0, 1]);
        s.next();
        assert_eq!(s.focused_line(), Some(1));
        s.next();
        assert_eq!(s.focused_line(), Some(0));
    }

    #[test]
    fn test_empty_query_finds_nothing() {
        let mut s = SearchState {
            query: String::new(),
            ..Default::default()
        };
        let t = vec![line("anything")];
        s.run(&t);
        assert!(s.matches.is_empty());
    }
}
