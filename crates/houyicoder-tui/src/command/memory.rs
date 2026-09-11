//! Memory commands and pane actions.
//!
//! Every row action resolves through MemoryPaneState so rendering and command
//! targets share one filtered selection.

use houyicoder_protocol::frontend::memory::MemoryToggleWhich;

use crate::agent_message::ClientCommand;
use crate::state::{App, Pane};

impl App {
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
            } else if let Some(req_id) = self.mint_request_id() {
                // The command form has no scope (the user typed a key); route
                // to the auto root, the original command-form behavior.
                self.send_cmd(ClientCommand::MemoryForgetQuery {
                    req_id,
                    key: key.to_string(),
                    scope: "auto".to_string(),
                });
                self.system_line(format!("memory: forgetting {key}..."));
            } else {
                self.system_line("memory: no carrier (stub mode)");
            }
            return true;
        }
        if let Some(req_id) = self.mint_request_id() {
            self.send_cmd(ClientCommand::MemoryShowQuery {
                req_id,
                key: body.to_string(),
            });
            self.system_line(format!("memory: fetching {body}..."));
        } else {
            self.system_line("memory: no carrier (stub mode)");
        }
        true
    }

    /// Toggle one background memory service from either the pane or command.
    pub(crate) fn toggle_memory_setting(&mut self, which: MemoryToggleWhich) {
        let label = match which {
            MemoryToggleWhich::Auto => "auto-memory",
            MemoryToggleWhich::Dream => "auto-dream",
        };
        if let Some(req_id) = self.mint_request_id() {
            self.send_cmd(ClientCommand::MemoryToggleQuery { req_id, which });
            self.system_line(format!("memory: toggling {label}..."));
        } else {
            self.system_line("memory: no carrier (stub mode)");
        }
    }

    pub fn cycle_memory_scope(&mut self) {
        self.memory.next_scope();
    }

    pub fn cycle_memory_scope_prev(&mut self) {
        self.memory.previous_scope();
    }

    pub fn move_memory_cursor(&mut self, delta: i32) {
        self.memory.move_cursor(delta);
    }

    /// Forget the memory row under the cursor (the d action). Sends the
    /// selected key to the server; the MemoryList reply refreshes the pane.
    /// No-op when no carrier or the filtered list is empty.
    pub fn forget_memory_at_cursor(&mut self) {
        let Some(memory) = self.memory.selected() else {
            return;
        };
        let key = memory.topic.clone();
        let scope = memory.scope.clone();
        if let Some(req_id) = self.mint_request_id() {
            // Route the delete by the row's scope so forgetting a
            // user/project row deletes the explicit file in that root, not
            // just the auto-scope copy.
            self.send_cmd(ClientCommand::MemoryForgetQuery { req_id, key, scope });
            self.system_line("memory: forgetting...".to_string());
        } else {
            self.system_line("memory: no carrier (stub mode)".to_string());
        }
    }

    /// Show the body of the memory row under the cursor (the enter action).
    /// Sends the selected key; the MemoryShow reply renders inline via the
    /// existing show path. No-op when no carrier or the filtered list is empty.
    pub fn show_memory_at_cursor(&mut self) {
        let Some(memory) = self.memory.selected() else {
            return;
        };
        let key = memory.topic.clone();
        if let Some(req_id) = self.mint_request_id() {
            self.memory.request_detail(req_id, key.clone());
            self.send_cmd(ClientCommand::MemoryShowQuery { req_id, key });
        } else {
            self.system_line("memory: no carrier (stub mode)".to_string());
        }
    }

    /// Set the text filter (the /memory search <term> command). Opens the pane
    /// and narrows the list to entries whose key or description match the term
    /// (case-insensitive), composed with the active scope tab. Resets the
    /// cursor so it never points past the narrowed list.
    pub fn set_memory_search(&mut self, term: &str) {
        self.pane = Pane::Memory;
        self.memory.set_search(term);
    }
}
