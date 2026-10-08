//! The transcript view's own state: the scroll, the search corpus, the
//! expansion sets, and the search view, as one value the view on screen
//! holds and a parked view keeps. Split from state.rs so the parent stays
//! under the file-size gate.
//!
//! This owner exists to hold the multi-field rules that the flat App fields
//! could only carry as comments: entering and leaving the search view is one
//! transaction rather than a dozen assignments a caller must repeat in order;
//! the two scroll objects are live exactly one at a time with a flag saying
//! which; the four expansion sets always hold the visible view's keys, so
//! parking and restoring a view swaps whole values instead of moving four
//! sets by hand. Fields arrive with their readers, group by group; the
//! scroll objects and the window flag land first, then the search corpus,
//! because the dispatch below is what makes the one-live-scroll rule a
//! method instead of a comment.

use crate::records::TranscriptLine;
use crate::scroll::TranscriptScroll;
use crate::state::App;
use crate::state::parked_view_states::ExpandedKeys;

/// The transcript view state the view on screen holds: where the reader is,
/// what they have expanded, and whether a search view is open over it.
///
/// Field privacy is the contract: callers outside this module reach state
/// through methods, so adding a field cannot silently widen what callers
/// outside the module can touch, the way the flat App fields did.
#[derive(Debug, Default)]
pub(crate) struct TranscriptViewState {
    /// Where the whole-vec transcript is scrolled to. Live exactly when the
    /// search view's byte-window mode is not; the window flag selects.
    scroll: TranscriptScroll,
    /// Within-window row scroll state for the search view's byte-window
    /// mode. Live exactly when that mode is on. A second instance of the
    /// same scroll type: the row math is shared, the row space is the
    /// window's alone.
    window_scroll: TranscriptScroll,
    /// True when the search view is in byte-window mode (log over the
    /// threshold). Renders flat through a separate path; see window_scroll.
    window_mode: bool,
    /// The frozen lines the search view renders and counts against, plus the
    /// load metadata that explains them. Pub(crate) rather than private like
    /// the scroll objects: the search's run path holds the corpus slice while
    /// it mutates the query state, a disjoint field borrow a method call
    /// cannot express.
    pub(crate) search_corpus: SearchCorpus,
    /// The byte-window the search view browses when the log is over the
    /// threshold: the loaded window's byte range, the frozen file size that
    /// bounds further reads, and the per-window corrupt-line skip count.
    history_window: HistoryWindow,
    /// The four open-row sets the visible view owns. Pub(crate) so the
    /// toggle and render paths reach a set directly; the whole value
    /// swaps on a view park or restore, which is the contract the flat
    /// fields could only carry as a comment.
    pub(crate) expanded: ExpandedKeys,
}

impl TranscriptViewState {
    /// The scroll object that currently holds the reader's position: the
    /// window's while byte-window mode is on, the whole-vec transcript's
    /// otherwise. Exactly one is live, and this is the flag's only read --
    /// a caller cannot re-derive which one from a bare boolean.
    pub(crate) fn scroll_on_view(&self) -> &TranscriptScroll {
        if self.window_mode {
            &self.window_scroll
        } else {
            &self.scroll
        }
    }

    /// The mutable form of scroll_on_view.
    pub(crate) fn scroll_on_view_mut(&mut self) -> &mut TranscriptScroll {
        if self.window_mode {
            &mut self.window_scroll
        } else {
            &mut self.scroll
        }
    }

    /// True while the search view is browsing a byte window rather than the
    /// loaded transcript. Behaviour branches on this (the window path pages
    /// by window loads, not by fold rows), so it is a read for callers that
    /// genuinely branch, not a second way to pick a scroll.
    pub(crate) fn window_browsing(&self) -> bool {
        self.window_mode
    }

    /// Enter or leave byte-window browsing. The search view's enter and
    /// exit transactions are the only writers.
    pub(crate) fn set_window_browsing(&mut self, on: bool) {
        self.window_mode = on;
    }

    /// The whole-vec transcript's scroll, regardless of which mode is on.
    /// Parent-only domain work (rebuild, trim, history reads) reads this
    /// explicitly and never the dispatch above: those paths act on the
    /// parent transcript even while a search view covers it.
    pub(crate) fn whole_scroll(&self) -> &TranscriptScroll {
        &self.scroll
    }

    /// The mutable form of whole_scroll.
    pub(crate) fn whole_scroll_mut(&mut self) -> &mut TranscriptScroll {
        &mut self.scroll
    }

    /// The search view's within-window scroll.
    pub(crate) fn window_scroll(&self) -> &TranscriptScroll {
        &self.window_scroll
    }

    /// The mutable form of window_scroll.
    pub(crate) fn window_scroll_mut(&mut self) -> &mut TranscriptScroll {
        &mut self.window_scroll
    }

    /// Reset the within-window scroll for a fresh window load: the new
    /// window's row space is unrelated to the old one's, so any surviving
    /// offset would be garbage.
    pub(crate) fn reset_window_scroll(&mut self) {
        self.window_scroll = TranscriptScroll::default();
    }

    /// Byte offset where the loaded window starts.
    pub(crate) fn window_anchor(&self) -> u64 {
        self.history_window.anchor
    }

    /// Byte offset past the loaded window's last line.
    pub(crate) fn window_end(&self) -> u64 {
        self.history_window.end
    }

    /// The log byte size frozen at enter; window reads stay in its bounds.
    pub(crate) fn frozen_file_size(&self) -> u64 {
        self.history_window.frozen_file_size
    }

    /// Corrupt lines skipped in the current window.
    pub(crate) fn window_skipped(&self) -> usize {
        self.history_window.skipped
    }

    /// Enter byte-window mode: freeze the file size and load the first window.
    pub(crate) fn enter_history_window(
        &mut self,
        frozen: u64,
        anchor: u64,
        end: u64,
        skipped: usize,
    ) {
        self.history_window.frozen_file_size = frozen;
        self.history_window.anchor = anchor;
        self.history_window.end = end;
        self.history_window.skipped = skipped;
    }

    /// Replace the window for a navigation step; the frozen size stays.
    pub(crate) fn replace_history_window(&mut self, anchor: u64, end: u64, skipped: usize) {
        self.history_window.anchor = anchor;
        self.history_window.end = end;
        self.history_window.skipped = skipped;
    }

    /// Exit: clear the window and the frozen boundary.
    pub(crate) fn exit_history_window(&mut self) {
        self.history_window.anchor = 0;
        self.history_window.end = 0;
        self.history_window.frozen_file_size = 0;
        self.history_window.skipped = 0;
    }
}

/// The frozen lines the search view renders and counts against, plus the
/// load metadata that explains them: whether the snapshot source declined the
/// whole log, the raw log byte size at enter, and the corrupt-line skip
/// count. One owner so enter and exit are transactions -- four assignments a
/// caller would otherwise repeat in lockstep, where forgetting one leaves
/// the chrome reading a stale value.
#[derive(Debug, Default)]
pub(crate) struct SearchCorpus {
    /// Frozen snapshot lines. Empty outside the search view; active_transcript
    /// picks it when search is active so count and render read one source.
    lines: Vec<TranscriptLine>,
    /// True when the snapshot source declined the whole log and the lines vec
    /// is empty as an honest degrade. The chrome shows a too-large hint
    /// instead of no-match.
    truncated: bool,
    /// The raw log byte size at enter, for the degrade hint. Zero when no
    /// snapshot source is wired.
    log_bytes: u64,
    /// Corrupt log lines the tolerant read skipped at enter. The chrome
    /// shows the count so dropped data is not a silent gap. Zero on the
    /// strict-replay path, which errors instead of skipping.
    skipped: usize,
}

impl SearchCorpus {
    /// The frozen snapshot lines the search view renders, counts, and matches
    /// against.
    pub(crate) fn lines(&self) -> &[TranscriptLine] {
        &self.lines
    }

    /// True when the snapshot source declined the whole log and the lines vec
    /// is empty as an honest degrade.
    pub(crate) fn truncated(&self) -> bool {
        self.truncated
    }

    /// The raw log byte size frozen at enter, for the degrade hint.
    pub(crate) fn log_bytes(&self) -> u64 {
        self.log_bytes
    }

    /// Corrupt log lines the tolerant read skipped at enter.
    pub(crate) fn skipped(&self) -> usize {
        self.skipped
    }

    /// Enter over-threshold: a byte window loaded, so the corpus is the
    /// window's lines, the frozen log size is the byte boundary, and skipped
    /// counts the window's corrupt lines. Not truncated -- the window is the
    /// real search view, not a degrade.
    pub(crate) fn enter_over_window(
        &mut self,
        lines: Vec<TranscriptLine>,
        log_bytes: u64,
        skipped: usize,
    ) {
        self.lines = lines;
        self.log_bytes = log_bytes;
        self.skipped = skipped;
        self.truncated = false;
    }

    /// Enter under-threshold: the whole log loaded, with its truncate and
    /// skip flags. No byte boundary applies.
    pub(crate) fn enter_under(
        &mut self,
        lines: Vec<TranscriptLine>,
        truncated: bool,
        skipped: usize,
    ) {
        self.lines = lines;
        self.truncated = truncated;
        self.skipped = skipped;
        self.log_bytes = 0;
    }

    /// Enter with no snapshot source: the corpus is a clone of the live
    /// transcript, no degrade, no boundary, no skips.
    pub(crate) fn enter_fallback(&mut self, lines: Vec<TranscriptLine>) {
        self.lines = lines;
        self.truncated = false;
        self.log_bytes = 0;
        self.skipped = 0;
    }

    /// Replace the lines for a newer or older window load during navigation.
    /// A window step changes which lines the view holds; the enter-time skip
    /// count the chrome shows stays.
    pub(crate) fn replace_lines(&mut self, lines: Vec<TranscriptLine>) {
        self.lines = lines;
    }

    /// Exit: clear the lines and the load metadata together.
    pub(crate) fn exit(&mut self) {
        self.lines.clear();
        self.truncated = false;
        self.log_bytes = 0;
        self.skipped = 0;
    }
}

/// The byte-window the search view browses when the log is over the
/// threshold: the loaded window's byte range, the frozen file size that
/// bounds further reads, and the per-window corrupt-line skip count. One
/// owner so enter, exit, and window navigation are transactions rather than
/// four assignments a caller repeats in lockstep.
#[derive(Debug, Default)]
struct HistoryWindow {
    /// Byte offset where the loaded window starts. For the byte-% position.
    anchor: u64,
    /// Byte offset past the loaded window's last line (== file size for the
    /// tail window). Scrolling newer loads a window starting here.
    end: u64,
    /// The log byte size frozen at enter. Window reads stay in [0, frozen)
    /// so events appended after enter are invisible (the window does not
    /// chase the growing tail).
    frozen_file_size: u64,
    /// Corrupt lines skipped in the current window (separate from the
    /// enter-time count the corpus holds, so window-mode chrome shows the
    /// per-window count).
    skipped: usize,
}

/// App's delegation to the transcript view-state owner. The owner field is
/// pub(crate), but these keep the call site short and give the reader's
/// position a stable name that does not change when fields move into the
/// owner. They live with the owner rather than in an App method grab-bag so
/// the delegation is one click away from what it reaches.
impl App {
    /// Whole-vec scroll; parent-only paths read this, not the dispatch.
    pub(crate) fn whole_scroll(&self) -> &TranscriptScroll {
        self.transcript_view.whole_scroll()
    }

    /// Mutable whole-vec scroll.
    pub(crate) fn whole_scroll_mut(&mut self) -> &mut TranscriptScroll {
        self.transcript_view.whole_scroll_mut()
    }

    /// Within-window scroll (the search view's own row space).
    pub(crate) fn window_scroll(&self) -> &TranscriptScroll {
        self.transcript_view.window_scroll()
    }

    /// Mutable within-window scroll.
    pub(crate) fn window_scroll_mut(&mut self) -> &mut TranscriptScroll {
        self.transcript_view.window_scroll_mut()
    }

    /// True while the search view browses a byte window.
    pub(crate) fn window_browsing(&self) -> bool {
        self.transcript_view.window_browsing()
    }

    /// Enter or leave byte-window browsing.
    pub(crate) fn set_window_browsing(&mut self, on: bool) {
        self.transcript_view.set_window_browsing(on);
    }

    /// Reset the within-window scroll for a fresh window load.
    pub(crate) fn reset_window_scroll(&mut self) {
        self.transcript_view.reset_window_scroll();
    }

    /// Byte offset where the loaded window starts.
    pub(crate) fn window_anchor(&self) -> u64 {
        self.transcript_view.window_anchor()
    }

    /// Byte offset past the loaded window's last line.
    pub(crate) fn window_end(&self) -> u64 {
        self.transcript_view.window_end()
    }

    /// The log byte size frozen at enter; window reads stay in its bounds.
    pub(crate) fn frozen_file_size(&self) -> u64 {
        self.transcript_view.frozen_file_size()
    }

    /// Corrupt lines skipped in the current window.
    pub(crate) fn window_skipped(&self) -> usize {
        self.transcript_view.window_skipped()
    }

    /// Enter byte-window mode: freeze the file size and load the first window.
    pub(crate) fn enter_history_window(
        &mut self,
        frozen: u64,
        anchor: u64,
        end: u64,
        skipped: usize,
    ) {
        self.transcript_view
            .enter_history_window(frozen, anchor, end, skipped);
    }

    /// Replace the window for a navigation step; the frozen size stays.
    pub(crate) fn replace_history_window(&mut self, anchor: u64, end: u64, skipped: usize) {
        self.transcript_view
            .replace_history_window(anchor, end, skipped);
    }

    /// Exit: clear the window and the frozen boundary.
    pub(crate) fn exit_history_window(&mut self) {
        self.transcript_view.exit_history_window();
    }
}
