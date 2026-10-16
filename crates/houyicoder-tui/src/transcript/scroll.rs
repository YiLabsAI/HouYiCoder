//! Transcript scrollback and search state. The transcript pane follows the
//! tail by default; PgUp breaks follow so older rows stay visible, and End
//! or a new user submission returns to the tail. A parked viewport is held by
//! a logical anchor -- the identity of the row it stands on -- so rows entering
//! or leaving above it move the viewport with the content instead of past it.
//! Search has two modes: an inline Ctrl+F bar that highlights matches in the
//! transcript, and a /search popup that lists matching lines and jumps the
//! scroll to the focused one.
//!
//! Layout metrics the key handler needs (cap, total rows) are published by the
//! pure view through Cell so the view stays a read-only renderer of App while
//! paging still knows the real viewport size. Resolving an anchor into a screen
//! row needs the row list, which the App holds, so the App runs the steps and
//! publishes the row it resolved; this module holds the position and the
//! published metrics and computes nothing that depends on the rows.

use std::cell::Cell;

use crate::input::InputField;
use crate::records::TranscriptLine;
use crate::row_id::RowId;
use crate::transcript::rows::NamedRows;

/// Cap on the in-memory viewable transcript. An unbounded buffer is the
/// perf cliff: the per-frame display-row count walks every line, and search
/// scans every line on each run, so a long session makes every frame O(total
/// history). The raw transcript stays on the session log for full-history
/// recall; this is the bounded viewable window, mirroring a native
/// terminal's bounded scrollback. Fold state is keyed by string (not line
/// index) so eviction does not corrupt it.
pub(crate) const VIEWABLE_SCROLLBACK_CAP: usize = 4000;

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
/// transcript exceeds the cap, and return how many were dropped. The search
/// view renders a frozen snapshot (active_transcript), not this live vec, so
/// an open search needs no recompute on eviction: its matches index into the
/// snapshot, which does not shift.
pub(crate) fn bound_scrollback(transcript: &mut NamedRows) -> usize {
    if transcript.len() > VIEWABLE_SCROLLBACK_CAP {
        let drop = transcript.len() - VIEWABLE_SCROLLBACK_CAP;
        transcript.drop_front(drop);
        drop
    } else {
        0
    }
}

/// New agent turns since the user scrolled away, as far as the resident frames
/// still show. is_lower_bound marks a count whose anchor frame was evicted:
/// turns before the resident front are no longer countable, so the count is
/// only what the window can still prove, and the label reads it as a floor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NewTurnCount {
    pub count: usize,
    pub is_lower_bound: bool,
}

/// Where a parked viewport stands: the row whose text holds the top line, and
/// how far the top stands into that row.
///
/// The row is named by identity rather than by its position in the row list, so
/// the same logical place survives rows entering above it (an older read landing
/// ahead of the resident rows, a drain releasing them) and rows leaving. The
/// offset counts Unicode scalars into the row's rendered text with the row
/// breaks between the rows of one identity, so it does not depend on where that
/// text wrapped when the anchor was taken: a re-wrap moves the same offset onto
/// the neighbouring screen row by the width of a swallowed space at most.
///
/// A viewport standing below every named row -- in the live tail, under text
/// still being written -- carries an offset past the end of the last named row
/// and reads back as that many rows below it. Such a place belongs to content
/// the agent is still producing, so the extension holds the parked row in view
/// at the bottom instead of snapping the viewport up to the last row an
/// identity can name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollAnchor {
    /// The identity of the row the top line stands in.
    pub row: RowId,
    /// The Unicode scalar offset within that row's text.
    pub char_offset: u32,
}

/// Scrollback state for the transcript pane: where the viewport stands, and the
/// viewport size the last draw published.
///
/// The anchor is the whole of the position -- None while the viewport follows
/// the tail, Some while it is parked on a named row. A separate follow flag
/// beside it would be a second statement of the same fact, and a reader and a
/// writer can then disagree about which one decides. Resolving the anchor into a
/// screen row needs the row list, which the App holds, so the resolved row is
/// published here rather than computed here.
#[derive(Debug, Clone)]
pub struct TranscriptScroll {
    /// The row the viewport is parked on. None follows the tail: new rows push
    /// in, and End or a new user submission restore it.
    ///
    /// Private: every flip routes through a method so the App wrapper can run
    /// the trim and scroll-away cleanup on a return to the tail, rather than
    /// the flip happening silently inside a scroll op.
    anchor: Option<ScrollAnchor>,
    /// The screen row the parked anchor last resolved to. A step publishes the
    /// row it lands on; a rebuild publishes the row the anchor resolved to over
    /// the rows it assembled. An anchor whose row is not resident keeps the last
    /// value, so the view holds its place while the older read that brings the
    /// row back runs.
    resolved: Cell<usize>,
    /// Viewport capacity in display rows, written by the view during draw so
    /// the key handler can page by a full screen. Defaults to a sane value so
    /// paging works before the first draw.
    pub cap: Cell<usize>,
    /// Total display-row count. The view writes it during draw; a scroll
    /// step before the first draw publishes the walked count here so the
    /// step clamps against the number the renderer will publish. The step
    /// and clamp methods read this cell and nothing else.
    pub total: Cell<usize>,
}

/// The outcome of a downward scroll step. Unchanged: already at the tail,
/// nothing moved. Parked: moved to a row further up, still scrolled back.
/// ReachedTail: crossed to the tail, so the caller trims the live transcript
/// and clears the scroll-away snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollTransition {
    Unchanged,
    Parked,
    ReachedTail,
}

impl Default for TranscriptScroll {
    fn default() -> Self {
        Self {
            anchor: None,
            resolved: Cell::new(0),
            cap: Cell::new(0),
            total: Cell::new(0),
        }
    }
}

impl TranscriptScroll {
    /// Whether the viewport follows the tail (new rows push in).
    pub fn is_following_tail(&self) -> bool {
        self.anchor.is_none()
    }

    /// The row the viewport is parked on, if it is parked at all.
    pub fn anchor(&self) -> Option<&ScrollAnchor> {
        self.anchor.as_ref()
    }

    /// Park the viewport on this row. The caller publishes the screen row the
    /// anchor resolves to, since resolving it needs the row list.
    pub fn park(&mut self, anchor: ScrollAnchor) {
        self.anchor = Some(anchor);
    }

    /// Publish the screen row the parked anchor resolved to.
    pub fn set_resolved_top(&self, row: usize) {
        self.resolved.set(row);
    }

    /// Effective cap (rows visible). Falls back to 1 when the view has not
    /// published a value yet so paging never divides by zero.
    pub fn effective_cap(&self) -> usize {
        let c = self.cap.get();
        if c == 0 { 1 } else { c }
    }

    /// Whether the row count leaves anything to scroll: a transcript that fits
    /// one viewport or less has no parked place to hold, so a step over it
    /// keeps following the tail instead of parking on the only screen there is.
    pub fn has_scroll_room(&self) -> bool {
        self.total.get() > self.effective_cap()
    }

    /// The top display-row index to render from. Follows the tail when nothing
    /// is parked; otherwise the row the anchor last resolved to, clamped to the
    /// valid range so the viewport always fills: never starts past total - cap.
    pub fn top_offset(&self) -> usize {
        let total_rows = self.total.get();
        let cap = self.effective_cap();
        if self.anchor.is_none() {
            total_rows.saturating_sub(cap)
        } else {
            self.resolved.get().min(total_rows.saturating_sub(cap))
        }
    }

    /// Number of rows hidden above the current viewport (the N in N more).
    pub fn more_above(&self) -> usize {
        self.top_offset()
    }

    /// Return to following the tail.
    pub fn follow_tail(&mut self) {
        self.anchor = None;
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
pub struct TranscriptSearch {
    /// True while the search view is active. Single source: read by the
    /// highlight gate, the chrome, and the active_transcript picker. The
    /// search view renders a frozen snapshot, not the live vec, so an open
    /// search needs no recompute when the live transcript is evicted past the
    /// cap.
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

impl TranscriptSearch {
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

    /// Open the search view over a query: the enter transaction's query
    /// half. The corpus, window, and scroll half lives with the caller,
    /// which owns those facts; this method owns only the query state.
    pub fn begin(&mut self, query: &str) {
        self.active = true;
        self.query = query.to_string();
    }

    /// Enter the in-view slash re-search bar, seeded with the current query.
    pub fn begin_input(&mut self) {
        self.input.set(self.query.clone());
        self.input_mode = true;
    }

    /// Commit the slash bar: the buffer becomes the stored query, the bar
    /// closes. The caller runs the match scan from the stored query so the
    /// corpus slice stays under its ownership.
    pub fn commit_input(&mut self) {
        self.query = self.input.value().to_string();
        self.input.clear();
        self.input_mode = false;
    }

    /// Cancel the slash bar: discard the buffer, keep the prior query and
    /// matches untouched.
    pub fn cancel_input(&mut self) {
        self.input.clear();
        self.input_mode = false;
    }

    /// Focus the newest match (the find-the-most-recent-discussion intent
    /// both the view entry and the slash-bar commit share). True when a
    /// match exists to focus.
    pub fn focus_newest(&mut self) -> bool {
        if self.matches.is_empty() {
            return false;
        }
        self.focus = self.matches.len() - 1;
        true
    }
}

#[cfg(test)]
mod tests;
