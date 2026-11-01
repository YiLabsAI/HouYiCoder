//! The /sandbox pane at its two tabs and three levels: what confines the
//! session, the writes the session recorded, one write's detail, and the
//! confirmation that puts its earlier bytes back.
//!
//! The listing is one row per recorded write followed by one row per record
//! the host could not judge, so the cursor index, the scrolling list, and the
//! two levels below it all count the same rows. A write the host could not
//! judge is listed with its reason instead of being left out or guessed at.

//! The detail compares the two sides the record kept, line by line, and says
//! which side it could not read rather than setting an empty one beside it.

mod boundaries;
mod checkpoints;
mod restore;

use std::path::Path;

use houyicoder_protocol::frontend::checkpoint::{
    CheckpointListView, CheckpointView, RestoreView, UnreadableView,
};
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::state::App;
use crate::ui::{CheckpointLevel, SandboxTab};
use crate::view::relative_time::{now_epoch_secs, relative_time};

/// The two tabs, in the order the left and right keys walk them.
const TABS: [(SandboxTab, &str); 2] = [
    (SandboxTab::Sandbox, "Sandbox"),
    (SandboxTab::Checkpoints, "Checkpoints"),
];

/// One row of the checkpoints listing: a write the host judged, or a record it
/// could not judge and why.
#[derive(Clone, Copy)]
pub(crate) enum CheckpointRow<'a> {
    /// A recorded write.
    Write(&'a CheckpointView),
    /// A record the host could not judge.
    Unreadable(&'a UnreadableView),
}

/// How many rows the listing holds, which the cursor and the renderer both
/// count.
pub(crate) fn row_count(list: &CheckpointListView) -> usize {
    list.checkpoints.len() + list.unreadable.len()
}

/// The row at an index of the listing, in the order the list draws them.
pub(crate) fn row_at(list: &CheckpointListView, index: usize) -> Option<CheckpointRow<'_>> {
    match list.checkpoints.get(index) {
        Some(view) => Some(CheckpointRow::Write(view)),
        None => list
            .unreadable
            .get(index.checked_sub(list.checkpoints.len())?)
            .map(CheckpointRow::Unreadable),
    }
}

/// What a restore answer says in the transcript, wherever the pane is. The
/// target is named when the listing still holds it, because the name the
/// record is stored under says nothing about which file changed.
pub(crate) fn restore_sentence(target: Option<&str>, outcome: &RestoreView) -> String {
    let what = target.unwrap_or("the target");
    match outcome {
        RestoreView::Unchanged => format!("checkpoint: {what} already held the earlier bytes"),
        RestoreView::Restored => format!("checkpoint: restored {what}"),
        RestoreView::Kept { copy: Some(copy) } => {
            format!("checkpoint: {what} changed since the write; the earlier bytes went to {copy}")
        }
        RestoreView::Kept { copy: None } => format!(
            "checkpoint: {what} changed since the write, which created it; nothing was put back"
        ),
        RestoreView::Unbacked => format!("checkpoint: the earlier bytes of {what} were never kept"),
    }
}

/// The file a target names, which is what tells one record from another on
/// screen. A target that ends in no file name is shown whole.
fn target_name(target: &str) -> String {
    Path::new(target)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| target.to_string())
}

/// When a write landed, relative to now. A record that never landed has no
/// time, which the row states instead of dating to the epoch.
fn written_label(written_ms: Option<u64>) -> String {
    match written_ms {
        Some(ms) => relative_time(now_epoch_secs(), ms / 1_000),
        None => "the write never landed".to_string(),
    }
}

pub(super) fn draw_content(f: &mut Frame, area: Rect, app: &App) {
    let pane = &app.ui.sandbox_pane;
    match (pane.tab.get(), pane.level.get()) {
        (SandboxTab::Sandbox, _) => boundaries::draw_boundaries(f, area, app),
        (SandboxTab::Checkpoints, CheckpointLevel::Detail) => {
            checkpoints::draw_detail(f, area, app)
        }
        (SandboxTab::Checkpoints, CheckpointLevel::Restore) => restore::draw_restore(f, area, app),
        (SandboxTab::Checkpoints, CheckpointLevel::List) => checkpoints::draw_list(f, area, app),
    }
}

#[cfg(test)]
mod tests;
