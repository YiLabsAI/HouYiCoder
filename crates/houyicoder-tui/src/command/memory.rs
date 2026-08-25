//! /memory sub-command dispatch + pane-action methods on App. Extracted from
//! command.rs so that file stays under the file-size gate. The methods are
//! the in-TUI surface: the toggle / forget sub-commands (typed in the input
//! box) + the pane cursor + d action (key-driven). They share the filtered
//! list with the render path so the cursor the user sees is the one the
//! action targets.

use houyicoder_protocol::frontend::memory::MemoryToggleWhich;

use crate::state::App;
use crate::state::enums::CyclicTab;

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
                Some(which) => {
                    let label = match which {
                        MemoryToggleWhich::Auto => "auto-memory",
                        MemoryToggleWhich::Dream => "auto-dream",
                    };
                    if let Some(req_id) = self.mint_request_id() {
                        self.send_cmd(crate::run_control::ClientCommand::MemoryToggleQuery {
                            req_id,
                            which,
                        });
                        self.system_line(format!("memory: toggling {label}..."));
                    } else {
                        self.system_line("memory: no carrier (stub mode)");
                    }
                }
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
                self.send_cmd(crate::run_control::ClientCommand::MemoryForgetQuery {
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
            self.send_cmd(crate::run_control::ClientCommand::MemoryShowQuery {
                req_id,
                key: body.to_string(),
            });
            self.system_line(format!("memory: fetching {body}..."));
        } else {
            self.system_line("memory: no carrier (stub mode)");
        }
        true
    }

    /// Cycle the /memory pane scope filter forward (Right arrow): All to
    /// User to Project to Auto to All. Pure client state — the list is
    /// already in memory, so the filter narrows without a wire round-trip;
    /// the next render shows the narrowed set. Resets the cursor so it
    /// never points past the new list.
    pub fn cycle_memory_scope(&mut self) {
        self.memory_scope_tab = self.memory_scope_tab.next();
        self.memory_list.cursor = 0;
    }

    /// Cycle the /memory pane scope filter backward (Left arrow).
    /// See cycle_memory_scope.
    pub fn cycle_memory_scope_prev(&mut self) {
        self.memory_scope_tab = self.memory_scope_tab.prev();
        self.memory_list.cursor = 0;
    }

    /// Move the /memory pane cursor one row up/down, clamped to the
    /// scope-filtered list. No-op when the filtered list is empty. Delegates
    /// the clamp + move to ListPaneState so the logic is shared with the
    /// worktree pane (and any future list pane).
    pub fn move_memory_cursor(&mut self, delta: i32) {
        let n = crate::command::render::filtered_memory(
            &self.memory_entries,
            self.memory_scope_tab,
            &self.memory_list.query,
        )
        .len();
        self.memory_list.move_cursor(delta, n);
    }

    /// Forget the memory row under the cursor (the d action). Sends the
    /// selected key to the server; the MemoryList reply refreshes the pane.
    /// No-op when no carrier or the filtered list is empty.
    pub fn forget_memory_at_cursor(&mut self) {
        let (key, scope) = match crate::command::render::filtered_memory(
            &self.memory_entries,
            self.memory_scope_tab,
            &self.memory_list.query,
        )
        .get(self.memory_list.cursor)
        {
            Some(m) => (m.topic.clone(), m.scope.clone()),
            None => return,
        };
        if let Some(req_id) = self.mint_request_id() {
            // Route the delete by the row's scope so forgetting a
            // user/project row deletes the explicit file in that root, not
            // just the auto-scope copy.
            self.send_cmd(crate::run_control::ClientCommand::MemoryForgetQuery {
                req_id,
                key,
                scope,
            });
            self.system_line("memory: forgetting...".to_string());
        } else {
            self.system_line("memory: no carrier (stub mode)".to_string());
        }
    }

    /// Show the body of the memory row under the cursor (the enter action).
    /// Sends the selected key; the MemoryShow reply renders inline via the
    /// existing show path. No-op when no carrier or the filtered list is empty.
    pub fn show_memory_at_cursor(&mut self) {
        let key = match crate::command::render::filtered_memory(
            &self.memory_entries,
            self.memory_scope_tab,
            &self.memory_list.query,
        )
        .get(self.memory_list.cursor)
        {
            Some(m) => m.topic.clone(),
            None => return,
        };
        if let Some(req_id) = self.mint_request_id() {
            self.send_cmd(crate::run_control::ClientCommand::MemoryShowQuery { req_id, key });
        } else {
            self.system_line("memory: no carrier (stub mode)".to_string());
        }
    }

    /// Set the text filter (the /memory search <term> command). Opens the pane
    /// and narrows the list to entries whose key or description match the term
    /// (case-insensitive), composed with the active scope tab. Resets the
    /// cursor so it never points past the narrowed list.
    pub fn set_memory_search(&mut self, term: &str) {
        self.pane = crate::state::Pane::Memory;
        self.memory_list.query = term.to_string();
        self.memory_list.cursor = 0;
    }
}
