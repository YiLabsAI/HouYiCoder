//! The transcript view's own state: the scroll, the expansion sets, and the
//! search view, as one value the view on screen holds and a parked view
//! keeps. Split from state.rs so the parent stays under the file-size gate.
//!
//! This owner exists to hold the multi-field rules that the flat App fields
//! could only carry as comments: entering and leaving the search view is one
//! transaction rather than a dozen assignments a caller must repeat in order;
//! the two scroll objects are live exactly one at a time with a flag saying
//! which; the four expansion sets always hold the visible view's keys, so
//! parking and restoring a view swaps whole values instead of moving four
//! sets by hand. Fields arrive with their readers, group by group; the
//! scroll objects and the window flag land first because the dispatch below
//! is what makes the one-live-scroll rule a method instead of a comment.

use crate::scroll::TranscriptScroll;
use crate::state::App;

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
}
