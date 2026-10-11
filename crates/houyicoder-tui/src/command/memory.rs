//! Memory commands and pane actions.
//!
//! Row actions compose the session's memory capability (the entries, the
//! toggle states, the pending mutations) with the Ui pane state (the scope
//! tab, the cursor, the search query, the detail drill) so rendering and
//! command targets share one filtered selection.

use houyicoder_protocol::frontend::memory::MemoryToggleWhich;

use crate::agent_message::ClientCommand;
use crate::evidence::MemoryEntry;
use crate::memory_state::toggle_label;
use crate::session::EnqueueError;
use crate::state::{App, Pane};

impl App {
    /// The rows the /memory pane shows: the session's entries narrowed by the
    /// pane's scope tab and search query. One composition of the two owners
    /// so the view, the cursor step, and the row actions filter identically.
    pub(crate) fn memory_filtered(&self) -> Vec<&MemoryEntry> {
        self.ui
            .memory
            .filtered(&self.sessions.current.capabilities.memory.entries)
    }

    /// The row under the pane cursor, or None when the filtered list is empty
    /// or the cursor sits past its end.
    pub(crate) fn memory_selected(&self) -> Option<&MemoryEntry> {
        self.memory_filtered().get(self.ui.memory.cursor()).copied()
    }

    /// Replace the session's entry list (a list reply) and park the pane
    /// cursor so it never points past the new rows.
    pub(crate) fn set_memory_entries(&mut self, entries: Vec<MemoryEntry>) {
        self.sessions.current.capabilities.memory.entries = entries;
        self.ui.memory.set_cursor(0);
    }

    /// Sweep every in-flight memory mark when the connection dies: the
    /// capability's pending mutations and the pane's loading detail. No reply
    /// can ever resolve them, and a sticky pending mark would refuse its
    /// action forever.
    pub(crate) fn clear_memory_pending(&mut self) {
        self.sessions.current.capabilities.memory.clear_actions();
        self.ui.memory.drop_loading_detail();
    }

    /// Run a /memory sub-command whose body follows the leading token. The
    /// body is the toggle form, the forget form, or a bare key (fetch the body
    /// for inline show). Returns true when handled. The argless /memory form
    /// never reaches here — it falls through to SlashCommand::Memory, which
    /// opens the pane.
    pub(crate) fn run_memory_subcommand(&mut self, body: &str) -> bool {
        if let Some(which_arg) = body.strip_prefix("toggle ").map(str::trim) {
            let which = match which_arg {
                "auto" => Some(MemoryToggleWhich::Auto),
                "dream" => Some(MemoryToggleWhich::Dream),
                _ => None,
            };
            match which {
                Some(which) => self.toggle_memory_setting(which),
                None => self.system_line("memory: usage /memory toggle auto|dream"),
            }
            return true;
        }
        // /memory search <term>: narrow the list by key + description
        // substring (composed with the active scope tab). Pure client state.
        if let Some(term) = body.strip_prefix("search ").map(str::trim) {
            if term.is_empty() {
                self.system_line("memory: usage /memory search <term>");
            } else {
                self.set_memory_search(term);
                self.system_line(format!("memory: filtering for {term}..."));
            }
            return true;
        }
        // /memory forget <key>: archive one memory by key (the command form of
        // the pane d action). The server replies with the refreshed list.
        if let Some(key) = body.strip_prefix("forget ").map(str::trim) {
            if key.is_empty() {
                self.system_line("memory: usage /memory forget <key>");
            } else {
                let Some(s) = self.sessions.current.connection.as_ref() else {
                    self.system_line("memory: not connected");
                    return true;
                };
                let Ok(req_id) = s.next_request_id() else {
                    self.system_line("memory: request ids exhausted");
                    return true;
                };
                // The command form has no scope (the user typed a key); route
                // to the auto root, the original command-form behavior. A
                // repeat forget of a key already in flight sends nothing.
                let registered = self
                    .sessions
                    .current
                    .capabilities
                    .memory
                    .begin_forget(req_id, key.to_string());
                if registered
                    && self
                        .enqueue(ClientCommand::MemoryForgetQuery {
                            req_id,
                            key: key.to_string(),
                            scope: "auto".to_string(),
                        })
                        .is_err()
                {
                    // The driver is gone: the delete was not sent. Roll the
                    // pending mark back so the key is not stuck in flight.
                    self.sessions
                        .current
                        .capabilities
                        .memory
                        .take_forget(req_id);
                    self.system_line(format!("couldn't forget {key} — connection lost"));
                }
            }
            return true;
        }
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("memory: not connected");
            return true;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("memory: request ids exhausted");
            return true;
        };
        if self
            .enqueue(ClientCommand::MemoryShowQuery {
                req_id,
                key: body.to_string(),
            })
            .is_ok()
        {
            self.system_line(format!("memory: fetching {body}..."));
        } else {
            self.system_line(format!("memory: couldn't fetch {body} — connection lost"));
        }
        true
    }

    /// Toggle one background memory service from either the pane or command.
    /// The pending flip shows in the pane header; the transcript gets the
    /// outcome only, once the reply lands. A repeat press on a switch that
    /// is already flipping is dropped so fast keypresses cannot race
    /// on→off→on against each other.
    pub(crate) fn toggle_memory_setting(&mut self, which: MemoryToggleWhich) {
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("memory: not connected");
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("memory: request ids exhausted");
            return;
        };
        if !self
            .sessions
            .current
            .capabilities
            .memory
            .begin_toggle(req_id, which)
        {
            return;
        }
        if let Err(EnqueueError::Closed) =
            self.enqueue(ClientCommand::MemoryToggleQuery { req_id, which })
        {
            // The driver is gone: the flip was not sent. Roll the pending
            // mark back — otherwise the switch refuses presses forever.
            self.sessions
                .current
                .capabilities
                .memory
                .take_toggle(req_id);
            self.system_line(format!(
                "couldn't toggle {} — connection lost",
                toggle_label(which)
            ));
        }
    }

    pub fn cycle_memory_scope(&mut self) {
        self.ui.memory.next_scope();
    }

    pub fn cycle_memory_scope_prev(&mut self) {
        self.ui.memory.previous_scope();
    }

    pub fn move_memory_cursor(&mut self, delta: i32) {
        let len = self.memory_filtered().len();
        self.ui.memory.move_cursor(delta, len);
    }

    /// Forget the memory row under the cursor (the d action). Sends the
    /// selected key to the server; the MemoryList reply refreshes the pane
    /// and writes the outcome. Returns without mutation when disconnected,
    /// the list is empty, or
    /// the same key already has a forget in flight.
    pub fn forget_memory_at_cursor(&mut self) {
        let Some(memory) = self.memory_selected() else {
            return;
        };
        let key = memory.topic.clone();
        let scope = memory.scope.clone();
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("memory: not connected".to_string());
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("memory: request ids exhausted");
            return;
        };
        if !self
            .sessions
            .current
            .capabilities
            .memory
            .begin_forget(req_id, key.clone())
        {
            return;
        }
        // Route the delete by the row's scope so forgetting a
        // user/project row deletes the explicit file in that root, not
        // just the auto-scope copy.
        if let Err(EnqueueError::Closed) = self.enqueue(ClientCommand::MemoryForgetQuery {
            req_id,
            key: key.clone(),
            scope,
        }) {
            self.sessions
                .current
                .capabilities
                .memory
                .take_forget(req_id);
            self.system_line(format!("couldn't forget {key} — connection lost"));
        }
    }

    /// Show the body of the memory row under the cursor (the enter action).
    /// Sends the selected key; the MemoryShow reply renders inline via the
    /// existing show path. Returns without mutation when disconnected or the
    /// filtered list is empty.
    pub fn show_memory_at_cursor(&mut self) {
        let Some(memory) = self.memory_selected() else {
            return;
        };
        let key = memory.topic.clone();
        let Some(s) = self.sessions.current.connection.as_ref() else {
            self.system_line("memory: not connected".to_string());
            return;
        };
        let Ok(req_id) = s.next_request_id() else {
            self.system_line("memory: request ids exhausted");
            return;
        };
        self.ui.memory.request_detail(req_id, key.clone());
        if let Err(EnqueueError::Closed) = self.enqueue(ClientCommand::MemoryShowQuery {
            req_id,
            key: key.clone(),
        }) {
            self.ui.memory.close_detail();
            self.system_line(format!("couldn't show {key} — connection lost"));
        }
    }

    /// Set the text filter (the /memory search <term> command). Opens the pane
    /// and narrows the list to entries whose key or description match the term
    /// (case-insensitive), composed with the active scope tab. Resets the
    /// cursor so it never points past the narrowed list.
    pub fn set_memory_search(&mut self, term: &str) {
        self.ui.navigation.pane = Pane::Memory;
        self.ui.memory.set_search(term);
    }
}
