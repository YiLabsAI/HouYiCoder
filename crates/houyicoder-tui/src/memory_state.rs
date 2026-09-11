//! State and transitions for the memory browser.
//!
//! The owner keeps list selection, filtering, toggle state, detail requests,
//! and bounded detail scrolling consistent across command, key, wire, and view
//! boundaries.

use std::cell::Cell;

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::memory::{MemoryDetail, ToggleState};

use crate::evidence::MemoryEntry;
use crate::list_pane_state::ListPaneState;
use crate::state::enums::{CyclicTab, MemoryScopeTab};

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

pub(crate) struct MemoryPaneState {
    entries: Vec<MemoryEntry>,
    toggles: ToggleState,
    scope: MemoryScopeTab,
    list: ListPaneState,
    detail: Option<MemoryDetailState>,
}

impl MemoryPaneState {
    pub(crate) fn new(entries: Vec<MemoryEntry>) -> Self {
        Self {
            entries,
            toggles: ToggleState {
                auto_memory: true,
                auto_dream: true,
            },
            scope: MemoryScopeTab::All,
            list: ListPaneState::default(),
            detail: None,
        }
    }

    pub(crate) fn entries(&self) -> &[MemoryEntry] {
        &self.entries
    }

    #[cfg(test)]
    pub(crate) fn clear_entries(&mut self) {
        self.entries.clear();
        self.list.cursor = 0;
    }

    pub(crate) fn toggles(&self) -> &ToggleState {
        &self.toggles
    }

    pub(crate) fn set_toggles(&mut self, toggles: ToggleState) {
        self.toggles = toggles;
    }

    pub(crate) fn scope(&self) -> MemoryScopeTab {
        self.scope
    }

    pub(crate) fn filtered(&self) -> Vec<&MemoryEntry> {
        let needle = self.list.query.to_ascii_lowercase();
        self.entries
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

    pub(crate) fn set_entries(&mut self, entries: Vec<MemoryEntry>) {
        self.entries = entries;
        self.list.cursor = 0;
    }

    pub(crate) fn cursor(&self) -> usize {
        self.list.cursor
    }

    #[cfg(test)]
    pub(crate) fn set_cursor(&mut self, cursor: usize) {
        self.list.cursor = cursor;
    }

    pub(crate) fn move_cursor(&mut self, delta: i32) {
        let len = self.filtered().len();
        self.list.move_cursor(delta, len);
    }

    pub(crate) fn selected(&self) -> Option<&MemoryEntry> {
        self.filtered().get(self.list.cursor).copied()
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
}
