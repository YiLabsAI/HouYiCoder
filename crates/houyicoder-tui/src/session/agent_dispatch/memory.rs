//! Applies the memory domain's inbound messages: the /memory pane's
//! list/detail/toggle replies, memory-change broadcasts, and the outcome
//! lines for rejected mutations. Request ids route each reply to the action
//! that asked.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::memory::{
    MemoryChange, MemoryChangeCausality, MemoryChangeId, MemoryChangeOrigin, MemoryDetail,
    MemoryOperation, MemorySummaryEntry, MemoryToggleWhich, ToggleState,
};

use crate::agent_message::ClientCommand;
use crate::command::render::{memory_entries_from_wire, render_memory_entry};
use crate::memory_state::toggle_label;
use crate::session::capabilities::MemoryAction;
use crate::state::{App, Pane};

impl App {
    /// Apply a memory-list reply. A forget answers with the refreshed list;
    /// the req_id match against a registered action tells it from a plain
    /// refresh, which writes no transcript.
    pub(crate) fn apply_memory_list(
        &mut self,
        req_id: RequestId,
        entries: Vec<MemorySummaryEntry>,
    ) {
        let forgot = self
            .sessions
            .current
            .capabilities
            .memory
            .take_forget(req_id);
        self.set_memory_entries(memory_entries_from_wire(&entries));
        if let Some(key) = forgot {
            self.system_line(format!("forgot {key}"));
        }
    }

    /// Apply a memory-detail reply: into the pane when the pane asked and the
    /// request is still pending, otherwise rendered as one transcript line.
    pub(crate) fn apply_memory_show(&mut self, req_id: RequestId, entry: Option<MemoryDetail>) {
        if self.ui.navigation.pane == Pane::Memory && self.ui.memory.is_pending(req_id) {
            let missing = entry.is_none();
            self.ui.memory.apply_detail(req_id, entry);
            if missing {
                self.system_line("memory: no such key");
            }
        } else if self.ui.navigation.pane != Pane::Memory && self.ui.memory.pending_key().is_some()
        {
            self.ui.memory.close_detail();
        } else if self.ui.memory.pending_key().is_none() {
            match entry {
                Some(entry) => self.system_line(render_memory_entry(&entry)),
                None => self.system_line("memory: no such key"),
            }
        }
    }

    /// Apply a toggle-pair reply. A flip and a pane-open read answer with the
    /// same shape; only the req_id a pending toggle was registered under
    /// writes the outcome line.
    ///
    /// The reply also refreshes the cached status snapshot, which is where the
    /// Config tab reads the two memory switches and the recap pair. Without
    /// that write the row would wait for the next periodic status poll, so a
    /// press would appear to take effect in the transcript before the pane
    /// showed it.
    pub(crate) fn apply_memory_toggles(&mut self, req_id: RequestId, state: ToggleState) {
        let memory = &mut self.sessions.current.capabilities.memory;
        let outcome = if let Some(which) = memory.take_toggle(req_id) {
            let on = match which {
                MemoryToggleWhich::Auto => state.auto_memory,
                MemoryToggleWhich::Dream => state.auto_dream,
            };
            Some(format!(
                "{} {}",
                toggle_label(which),
                if on { "on" } else { "off" }
            ))
        } else if memory.take_recap(req_id) {
            let switch = if state.recap { "on" } else { "off" };
            Some(format!(
                "recap: {switch} · idle after {}m",
                state.recap_idle_secs / 60
            ))
        } else {
            None
        };
        self.sessions
            .current
            .capabilities
            .memory
            .set_toggles(state.clone());
        self.record_setting_toggles(&state);
        if let Some(line) = outcome {
            self.system_line(line);
        }
    }

    /// Fold the settings-file switches a reply carries into the cached status
    /// snapshot, so the Config tab and the status bar read the value the host
    /// just wrote. A reply that lands before the first status poll seeds the
    /// cache from the same stub the pane already renders, so the next press
    /// reads the confirmed value rather than the stub's default.
    fn record_setting_toggles(&mut self, state: &ToggleState) {
        let stub = self.snapshot_or_stub();
        let snapshot = self
            .sessions
            .current
            .capabilities
            .status
            .status_cache
            .get_or_insert(stub);
        snapshot.auto_memory = state.auto_memory;
        snapshot.auto_dream = state.auto_dream;
        snapshot.recap_enabled = state.recap;
        snapshot.recap_idle_secs = state.recap_idle_secs;
    }

    /// Render a memory-change broadcast and refresh the open pane. The change
    /// id dedupes replays: a broadcast already registered is dropped rather
    /// than shown twice.
    ///
    /// The notice carries its summary as the first line and each changed key
    /// on a child row; the fold layer collapses it to the summary by default
    /// and Ctrl+O or a click reveals the keys. Keep the keys in the notice so
    /// the detail is reachable in the transcript, matching how a tool-call
    /// group hides its calls behind a summary.
    pub(crate) fn show_memory_changes(
        &mut self,
        id: &MemoryChangeId,
        origin: MemoryChangeOrigin,
        _causality: MemoryChangeCausality,
        changes: &[MemoryChange],
    ) {
        if !self
            .sessions
            .current
            .capabilities
            .memory
            .register_change(id)
        {
            return;
        }
        let count = changes.len();
        let noun = if count == 1 { "change" } else { "changes" };
        let producer = match origin {
            MemoryChangeOrigin::PrimaryAgent => "saved by the agent",
            MemoryChangeOrigin::AutoMemory => "extracted in background",
            MemoryChangeOrigin::AutoDream => "consolidated in background",
            MemoryChangeOrigin::Unknown => "changed",
        };
        let mut notice = format!("Memory {producer}: {count} {noun} · /memory");
        for change in changes {
            notice.push_str(&format!(
                "\n  ⎿  {} {}",
                operation_verb(change.operation),
                change.key
            ));
            if let Some(scope) = change.scope.label() {
                notice.push_str(&format!(" · scope: {scope}"));
            }
        }
        // The transcript is the durable record of what happened; the pane is
        // the live view of what exists now. A memory change is permanent, so
        // the notice always lands in the transcript, and the open pane also
        // refreshes to show the new state.
        self.system_line(notice);
        if self.ui.navigation.pane == Pane::Memory
            && let Some(s) = self.sessions.current.connection.as_ref()
        {
            match s.next_request_id() {
                Ok(req_id) => {
                    self.enqueue_refresh(ClientCommand::MemoryListQuery { req_id });
                }
                Err(_) => self.note_request_id_exhausted(),
            }
        }
    }

    /// Route a failed memory pane mutation: the action context stays in the
    /// outcome line so it says what did not happen instead of a bare error.
    pub(crate) fn memory_failure_line(action: MemoryAction, message: &str) -> String {
        match action {
            MemoryAction::Toggle { which } => {
                format!("couldn't toggle {} — {message}", toggle_label(which))
            }
            MemoryAction::Recap => {
                format!("couldn't update recap — {message}")
            }
            MemoryAction::Forget { key } => {
                format!("couldn't forget {key} — {message}")
            }
        }
    }
}

/// The verb for one operation in the notice's child rows.
fn operation_verb(operation: MemoryOperation) -> &'static str {
    match operation {
        MemoryOperation::Created => "created",
        MemoryOperation::Updated => "updated",
        MemoryOperation::Deleted => "deleted",
        MemoryOperation::Promoted => "promoted",
        MemoryOperation::Demoted => "demoted",
        MemoryOperation::Unknown => "changed",
    }
}
