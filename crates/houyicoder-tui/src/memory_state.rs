//! Interaction state for the memory browser pane.
//!
//! The owner keeps the scope tab, the list cursor and search query, and the
//! bounded detail scrolling consistent across command, key, and view
//! boundaries. The session's entry list, toggle states, change-notice
//! identity, and in-flight mutations live on the memory capability; the state
//! here is what the user is looking at, not what the session knows, and the
//! compositions across the two owners are App methods.

use std::cell::Cell;

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::memory::{MemoryDetail, MemoryToggleWhich};

use crate::evidence::MemoryEntry;
use crate::list_pane_state::ListPaneState;
use crate::state::enums::{CyclicTab, MemoryScopeTab};

/// The user-facing name of one toggle switch, shared by the pane header,
/// the transcript outcomes, and the command layer.
pub(crate) fn toggle_label(which: MemoryToggleWhich) -> &'static str {
    match which {
        MemoryToggleWhich::Auto => "auto-memory",
        MemoryToggleWhich::Dream => "auto-dream",
    }
}

pub(crate) enum MemoryDetailState {
    Loading {
        request_id: RequestId,
        key: String,
    },
    Open {
        entry: MemoryDetail,
        offset: Cell<u16>,
        max_offset: Cell<u16>,
    },
}

/// The /memory pane's interaction state: the scope tab filter, the list
/// cursor and search query, and the detail drill-down (a loading request or
/// an open body with its scroll position). The rows these index are the
/// session's memory capability; a session switch resets this value with its
/// pane peers.
#[derive(Default)]
pub(crate) struct MemoryPaneState {
    scope: MemoryScopeTab,
    list: ListPaneState,
    detail: Option<MemoryDetailState>,
}

impl MemoryPaneState {
    pub(crate) fn scope(&self) -> MemoryScopeTab {
        self.scope
    }

    /// The rows the pane shows: the session's entries narrowed by the scope
    /// tab and the search query. The entries are borrowed from the capability
    /// so the caller composes the two owners.
    pub(crate) fn filtered<'a>(&self, entries: &'a [MemoryEntry]) -> Vec<&'a MemoryEntry> {
        let needle = self.list.query.to_ascii_lowercase();
        entries
            .iter()
            .filter(|memory| {
                self.scope == MemoryScopeTab::All || memory.scope == self.scope.label()
            })
            .filter(|memory| {
                needle.is_empty()
                    || memory.topic.to_ascii_lowercase().contains(&needle)
                    || memory.summary.to_ascii_lowercase().contains(&needle)
            })
            .collect()
    }

    pub(crate) fn cursor(&self) -> usize {
        self.list.cursor
    }

    pub(crate) fn set_cursor(&mut self, cursor: usize) {
        self.list.cursor = cursor;
    }

    /// Move the cursor by a signed delta within the filtered row count the
    /// caller composed from both owners.
    pub(crate) fn move_cursor(&mut self, delta: i32, len: usize) {
        self.list.move_cursor(delta, len);
    }

    pub(crate) fn next_scope(&mut self) {
        self.scope = self.scope.next();
        self.list.cursor = 0;
    }

    pub(crate) fn previous_scope(&mut self) {
        self.scope = self.scope.prev();
        self.list.cursor = 0;
    }

    pub(crate) fn set_search(&mut self, query: &str) {
        self.list.query = query.to_string();
        self.list.cursor = 0;
    }

    pub(crate) fn searching(&self) -> bool {
        self.list.searching()
    }

    pub(crate) fn search_query(&self) -> &str {
        &self.list.query
    }

    pub(crate) fn clear_search(&mut self) {
        self.list.clear_query();
        self.list.cursor = 0;
    }

    pub(crate) fn request_detail(&mut self, request_id: RequestId, key: String) {
        self.detail = Some(MemoryDetailState::Loading { request_id, key });
    }

    pub(crate) fn is_pending(&self, request_id: RequestId) -> bool {
        matches!(
            self.detail.as_ref(),
            Some(MemoryDetailState::Loading {
                request_id: pending,
                ..
            }) if *pending == request_id
        )
    }

    pub(crate) fn apply_detail(
        &mut self,
        request_id: RequestId,
        entry: Option<MemoryDetail>,
    ) -> bool {
        let Some(MemoryDetailState::Loading {
            request_id: pending,
            ..
        }) = self.detail.as_ref()
        else {
            return false;
        };
        if *pending != request_id {
            return false;
        }
        self.detail = entry.map(|entry| MemoryDetailState::Open {
            entry,
            offset: Cell::new(0),
            max_offset: Cell::new(0),
        });
        true
    }

    pub(crate) fn detail(&self) -> Option<&MemoryDetailState> {
        self.detail.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn detail_offset(&self) -> Option<u16> {
        match self.detail.as_ref() {
            Some(MemoryDetailState::Open { offset, .. }) => Some(offset.get()),
            _ => None,
        }
    }

    pub(crate) fn close_detail(&mut self) {
        self.detail = None;
    }

    pub(crate) fn set_detail_max_offset(&self, max_offset: u16) {
        if let Some(MemoryDetailState::Open {
            offset,
            max_offset: limit,
            ..
        }) = self.detail.as_ref()
        {
            limit.set(max_offset);
            offset.set(offset.get().min(max_offset));
        }
    }

    pub(crate) fn scroll_detail(&self, delta: i16) {
        let Some(MemoryDetailState::Open {
            offset, max_offset, ..
        }) = self.detail.as_ref()
        else {
            return;
        };
        let next = if delta.is_negative() {
            offset.get().saturating_sub(delta.unsigned_abs())
        } else {
            offset.get().saturating_add(delta.unsigned_abs())
        };
        offset.set(next.min(max_offset.get()));
    }

    pub(crate) fn pending_key(&self) -> Option<&str> {
        match self.detail.as_ref() {
            Some(MemoryDetailState::Loading { key, .. }) => Some(key),
            _ => None,
        }
    }

    /// Drop a detail still loading; an open detail is settled state, not a
    /// pending one, and survives. The Ui-side half of the connection-death
    /// cleanup, sweeping the pane beside the capability's pending mutations.
    pub(crate) fn drop_loading_detail(&mut self) {
        if matches!(self.detail, Some(MemoryDetailState::Loading { .. })) {
            self.detail = None;
        }
    }
}
