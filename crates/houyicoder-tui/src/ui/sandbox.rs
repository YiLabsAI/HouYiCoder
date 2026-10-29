//! The /sandbox pane state: which tab is open, how far the checkpoints
//! listing has drilled down, and the answers the host gave. Every answer is
//! matched to the request the pane sent, so a reply that lands after the user
//! moved on cannot rewrite the screen.

use std::cell::{Cell, Ref, RefCell};

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::checkpoint::{
    CheckpointListView, CheckpointTextView, SandboxBoundariesView,
};

use crate::ui::enums::CyclicTab;

/// Which half of the pane is on top.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxTab {
    /// What confines the session: the fence and the writable roots.
    #[default]
    Sandbox,
    /// Every write the session recorded, and the restore of one.
    Checkpoints,
}

impl CyclicTab for SandboxTab {
    const ORDER: &'static [Self] = &[Self::Sandbox, Self::Checkpoints];
}

/// How far into the checkpoints listing the pane has drilled down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CheckpointLevel {
    /// The list of recorded writes.
    #[default]
    List,
    /// One write, with both images the record kept.
    Detail,
    /// The confirmation that puts one write's earlier bytes back.
    Restore,
}

/// Which button the restore confirmation holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RestoreFocus {
    /// Leave the target as it is. The row starts here, so a restore takes one
    /// deliberate move.
    #[default]
    Cancel,
    /// Put the earlier bytes back.
    Restore,
}

/// The open tab, the listing's position, and the host's answers.
#[derive(Default)]
pub struct SandboxPaneState {
    /// Which half of the pane is on top.
    pub tab: Cell<SandboxTab>,
    /// How far into the listing the pane has drilled down.
    pub level: Cell<CheckpointLevel>,
    /// The row the listing holds, as an index into the writes followed by the
    /// unreadable records.
    pub cursor: Cell<usize>,
    /// How many rows the detail or the confirmation is scrolled into its
    /// block, matching the worktree pane's behaviour.
    pub scroll: Cell<u16>,
    /// Which button the restore confirmation holds.
    pub focus: Cell<RestoreFocus>,
    restore_from: Cell<CheckpointLevel>,
    boundaries: RefCell<Option<SandboxBoundariesView>>,
    checkpoints: RefCell<Option<CheckpointListView>>,
    texts: RefCell<Option<CheckpointTextView>>,
    boundaries_request: Cell<Option<RequestId>>,
    list_request: Cell<Option<RequestId>>,
    texts_request: Cell<Option<RequestId>>,
    restore_request: Cell<Option<RequestId>>,
    notice: RefCell<Option<String>>,
}

impl SandboxPaneState {
    /// Leave whatever the pane was showing and start again on the sandbox
    /// tab. An answer still on its way is dropped with the views, so it cannot
    /// land on the fresh screen.
    pub fn reset(&self) {
        self.tab.set(SandboxTab::Sandbox);
        self.set_level(CheckpointLevel::List);
        self.cursor.set(0);
        self.focus.set(RestoreFocus::Cancel);
        self.restore_from.set(CheckpointLevel::List);
        self.boundaries.replace(None);
        self.checkpoints.replace(None);
        self.texts.replace(None);
        self.boundaries_request.set(None);
        self.list_request.set(None);
        self.texts_request.set(None);
        self.restore_request.set(None);
        self.notice.replace(None);
    }

    /// Switch to a tab. A drill-down belongs to the listing, so the new tab
    /// starts at its own root.
    pub fn set_tab(&self, tab: SandboxTab) {
        if self.tab.get() == tab {
            return;
        }
        self.tab.set(tab);
        self.set_level(CheckpointLevel::List);
        self.cursor.set(0);
    }

    /// Move to another level of the listing, which starts its block at the
    /// top: a scroll position belongs to the block it was made in. The text a
    /// detail compared belongs to the row it was read for, so leaving the
    /// detail drops it and any answer still on its way.
    pub fn set_level(&self, level: CheckpointLevel) {
        self.level.set(level);
        self.scroll.set(0);
        if level != CheckpointLevel::Detail {
            self.texts.replace(None);
            self.texts_request.set(None);
        }
    }

    /// Open the restore confirmation for the row under the cursor, remembering
    /// the level it was opened from so leaving it returns there. Leaving the
    /// confirmation hands the buttons back to Cancel, so a second restore takes
    /// the same deliberate move as the first.
    pub fn begin_confirm(&self) {
        if self.level.get() == CheckpointLevel::Restore {
            return;
        }
        self.restore_from.set(self.level.get());
        self.focus.set(RestoreFocus::Cancel);
        self.set_level(CheckpointLevel::Restore);
    }

    /// The level the restore confirmation returns to.
    pub fn restore_from(&self) -> CheckpointLevel {
        self.restore_from.get()
    }

    /// Move the cursor by this many rows, held inside the listing.
    pub fn move_cursor(&self, delta: i32, rows: usize) {
        let next = i32::try_from(self.cursor.get())
            .unwrap_or(i32::MAX)
            .saturating_add(delta)
            .clamp(0, rows.saturating_sub(1) as i32);
        self.cursor.set(usize::try_from(next).unwrap_or(0));
    }

    /// Move the block one row down by this many rows. The renderer holds the
    /// position at the last row that still fills the block, so moving past the
    /// end stays here until the next way up.
    pub fn scroll_by(&self, delta: i32) {
        let rows = i32::from(self.scroll.get()).saturating_add(delta);
        self.scroll
            .set(u16::try_from(rows.max(0)).unwrap_or(u16::MAX));
    }

    /// Move the confirmation's focus to the other button.
    pub fn toggle_focus(&self) {
        self.focus.set(match self.focus.get() {
            RestoreFocus::Cancel => RestoreFocus::Restore,
            RestoreFocus::Restore => RestoreFocus::Cancel,
        });
    }

    /// The boundaries the host last answered.
    pub fn boundaries(&self) -> Option<SandboxBoundariesView> {
        self.boundaries.borrow().clone()
    }

    /// The listing the host last answered, borrowed so a render of many rows
    /// copies none of them.
    pub fn checkpoints(&self) -> Ref<'_, Option<CheckpointListView>> {
        self.checkpoints.borrow()
    }

    /// Whether a read of the boundaries is still out.
    pub fn reading_boundaries(&self) -> bool {
        self.boundaries_request.get().is_some()
    }

    /// Whether a read of the listing is still out.
    pub fn reading_checkpoints(&self) -> bool {
        self.list_request.get().is_some()
    }

    /// The request the pane waits on for the boundaries.
    pub fn boundaries_request(&self) -> Option<RequestId> {
        self.boundaries_request.get()
    }

    /// The request the pane waits on for the listing.
    pub fn list_request(&self) -> Option<RequestId> {
        self.list_request.get()
    }

    /// A boundaries read ended without an answer: the pane stops waiting and
    /// the next way to the tab reads again.
    pub fn abandon_boundaries(&self) {
        self.boundaries_request.set(None);
    }

    /// A listing read ended without an answer, for the same reason.
    pub fn abandon_list(&self) {
        self.list_request.set(None);
    }

    /// The request the pane waits on for a restore.
    pub fn restore_request(&self) -> Option<RequestId> {
        self.restore_request.get()
    }

    /// Record the request a boundaries read went out under.
    pub fn expect_boundaries(&self, request: RequestId) {
        self.boundaries_request.set(Some(request));
    }

    /// Record the request a listing read went out under.
    pub fn expect_list(&self, request: RequestId) {
        self.list_request.set(Some(request));
    }

    /// Take a boundaries answer. The caller has already matched it to the
    /// request the pane sent. A fresh answer replaces an older failure notice,
    /// which named a read the screen no longer needs to explain.
    pub fn accept_boundaries(&self, view: SandboxBoundariesView) {
        self.boundaries_request.set(None);
        self.notice.replace(None);
        self.boundaries.replace(Some(view));
    }

    /// Take a listing answer and hold the cursor inside it. The caller has
    /// already matched it to the request the pane sent.
    pub fn accept_checkpoints(&self, view: CheckpointListView) {
        self.list_request.set(None);
        self.notice.replace(None);
        let rows = view.checkpoints.len() + view.unreadable.len();
        if self.cursor.get() >= rows {
            self.cursor.set(rows.saturating_sub(1));
        }
        self.checkpoints.replace(Some(view));
    }

    /// Whether a read of the open row's two sides is still out.
    pub fn reading_texts(&self) -> bool {
        self.texts_request.get().is_some()
    }

    /// The request the pane waits on for the open row's two sides.
    pub fn texts_request(&self) -> Option<RequestId> {
        self.texts_request.get()
    }

    /// Record the request a text read went out under.
    pub fn expect_texts(&self, request: RequestId) {
        self.texts_request.set(Some(request));
    }

    /// Drop the two sides and any read of them still out, so a fresh read
    /// starts from nothing.
    pub fn forget_texts(&self) {
        self.texts.replace(None);
        self.texts_request.set(None);
    }

    /// A text read ended without an answer: the pane stops waiting.
    pub fn abandon_texts(&self) {
        self.texts_request.set(None);
    }

    /// Take a text answer. The caller has already matched it to the request
    /// the pane sent.
    pub fn accept_texts(&self, view: CheckpointTextView) {
        self.texts_request.set(None);
        self.texts.replace(Some(view));
    }

    /// The two sides the host last answered, borrowed so a render of the
    /// detail copies none of their bytes.
    pub fn texts(&self) -> Ref<'_, Option<CheckpointTextView>> {
        self.texts.borrow()
    }

    /// Record the request a restore went out under, and that nothing else may
    /// go out under the same gesture. None when a restore is already with the
    /// host.
    pub fn begin_restore(&self, request: RequestId) -> bool {
        if self.restore_request.get().is_some() {
            return false;
        }
        self.restore_request.set(Some(request));
        self.notice.replace(None);
        true
    }

    /// The restore ended, whatever the answer: a retry is possible again.
    pub fn finish_restore(&self, notice: String) {
        self.restore_request.set(None);
        self.notice.replace(Some(notice));
    }

    /// The restore ended and the pane stops waiting on it, without a sentence
    /// of its own: the outcome of a restore belongs in the transcript, which
    /// keeps it after the pane closes.
    pub fn end_restore(&self) {
        self.restore_request.set(None);
    }

    /// The host's last answer about a restore.
    pub fn notice(&self) -> Option<String> {
        self.notice.borrow().clone()
    }

    /// Record a sentence about this screen, without touching what the pane
    /// waits on.
    pub fn set_notice(&self, notice: String) {
        self.notice.replace(Some(notice));
    }
}

#[cfg(test)]
mod tests;
