//! State and transitions for the memory browser.
//!
//! The owner keeps list selection, filtering, toggle state, detail requests,
//! notice identity, and bounded detail scrolling consistent across command,
//! key, wire, and view boundaries.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::memory::{
    MemoryChangeId, MemoryDetail, MemoryToggleWhich, ToggleState,
};

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

/// An in-flight pane mutation, keyed by the request id its reply will carry.
/// Reads (list refresh, toggle-state fetch) never register an action, so only
/// a mutation's reply writes a transcript outcome.
pub(crate) enum MemoryAction {
    Toggle { which: MemoryToggleWhich },
    Forget { key: String },
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

pub(crate) struct MemoryPaneState {
    entries: Vec<MemoryEntry>,
    toggles: ToggleState,
    scope: MemoryScopeTab,
    list: ListPaneState,
    detail: Option<MemoryDetailState>,
    seen_changes: HashSet<MemoryChangeId>,
    actions: HashMap<RequestId, MemoryAction>,
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
            seen_changes: HashSet::new(),
            actions: HashMap::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn clear_entries(&mut self) {
        self.entries.clear();
        self.list.cursor = 0;
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[MemoryEntry] {
        &self.entries
    }

    pub(crate) fn toggles(&self) -> &ToggleState {
        &self.toggles
    }

    pub(crate) fn set_toggles(&mut self, toggles: ToggleState) {
        self.toggles = toggles;
    }

    /// Whether one switch has a flip in flight (the header's pending mark).
    pub(crate) fn toggle_pending(&self, which: MemoryToggleWhich) -> bool {
        self.actions.values().any(
            |action| matches!(action, MemoryAction::Toggle { which: pending } if *pending == which),
        )
    }

    /// Record an in-flight toggle flip. Returns false when the same switch
    /// already has one pending, so a repeat press is dropped instead of
    /// racing on→off→on; the other switch stays operable.
    pub(crate) fn begin_toggle(&mut self, req_id: RequestId, which: MemoryToggleWhich) -> bool {
        if self.toggle_pending(which) {
            return false;
        }
        self.actions.insert(req_id, MemoryAction::Toggle { which });
        true
    }

    /// Record an in-flight forget; the matching list reply writes the
    /// outcome. Returns false when the same key already has one pending, so
    /// a repeat d-press on a row ships a single delete.
    pub(crate) fn begin_forget(&mut self, req_id: RequestId, key: String) -> bool {
        let in_flight = self.actions.values().any(
            |action| matches!(action, MemoryAction::Forget { key: pending } if *pending == key),
        );
        if in_flight {
            return false;
        }
        self.actions.insert(req_id, MemoryAction::Forget { key });
        true
    }

    /// Claim the action a reply's req_id belongs to, clearing it. None for
    /// plain reads and refreshes — they write no transcript.
    pub(crate) fn take_action(&mut self, req_id: RequestId) -> Option<MemoryAction> {
        self.actions.remove(&req_id)
    }

    /// Claim an in-flight toggle flip. None when the id belongs to a forget,
    /// a plain read, or was already claimed — the other action's pending
    /// mark survives a mismatched claim.
    pub(crate) fn take_toggle(&mut self, req_id: RequestId) -> Option<MemoryToggleWhich> {
        match self.actions.get(&req_id) {
            Some(MemoryAction::Toggle { which }) => {
                let which = *which;
                self.actions.remove(&req_id);
                Some(which)
            }
            _ => None,
        }
    }

    /// Claim an in-flight forget. None when the id belongs to a toggle, a
    /// plain read, or was already claimed — the other action's pending mark
    /// survives a mismatched claim.
    pub(crate) fn take_forget(&mut self, req_id: RequestId) -> Option<String> {
        match self.actions.get(&req_id) {
            Some(MemoryAction::Forget { key }) => {
                let key = key.clone();
                self.actions.remove(&req_id);
                Some(key)
            }
            _ => None,
        }
    }

    /// Drop every in-flight mark: pending actions and a detail still
    /// loading. Called when the connection dies — no reply can ever resolve
    /// them, and a sticky pending mark would refuse the switch forever. An
    /// open detail is settled state, not a pending one, and survives.
    pub(crate) fn clear_pending(&mut self) {
        self.actions.clear();
        if matches!(self.detail, Some(MemoryDetailState::Loading { .. })) {
            self.detail = None;
        }
    }

    /// The keys with a forget in flight, sorted for deterministic assertions.
    #[cfg(test)]
    pub(crate) fn pending_forget_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .actions
            .values()
            .filter_map(|action| match action {
                MemoryAction::Forget { key } => Some(key.clone()),
                MemoryAction::Toggle { .. } => None,
            })
            .collect();
        keys.sort();
        keys
    }

    /// How many switches have a flip in flight, for repeat-press assertions.
    #[cfg(test)]
    pub(crate) fn pending_toggle_count(&self) -> usize {
        self.actions
            .values()
            .filter(|action| matches!(action, MemoryAction::Toggle { .. }))
            .count()
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

    pub(crate) fn register_change(&mut self, id: &MemoryChangeId) -> bool {
        self.seen_changes.insert(id.clone())
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
