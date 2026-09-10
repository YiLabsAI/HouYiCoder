//! Pending user input and deferred session commands.
//!
//! Queue order is local. At most one input is mirrored to the active runner.

use houyicoder_protocol::frontend::QueuedInput;

use crate::run_control::ClientCommand;
use crate::state::App;

/// One item awaiting commitment or local command execution.
#[derive(Debug, Clone)]
pub enum PendingItem {
    /// User input mirrored to the active runner.
    Message(QueuedInput),
    /// User input held only by the frontend.
    ParkedMessage(QueuedInput),
    /// A slash command's raw text, including the leading slash (e.g.
    /// "/resume <sid>", "/clear"). Stored verbatim so recall
    /// (pop_queued_to_input) re-fills the input box with the exact
    /// text the user typed, and the drain re-dispatches it. Never InjectUser'd.
    Command(String),
}

impl PartialEq for PendingItem {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Message(a), Self::Message(b))
            | (Self::ParkedMessage(a), Self::ParkedMessage(b)) => a.text == b.text,
            (Self::Command(a), Self::Command(b)) => a == b,
            _ => false,
        }
    }
}

impl PendingItem {
    /// The text to show in the queue strip: the message body, or the
    /// command text (with the slash the user typed).
    pub fn display(&self) -> &str {
        match self {
            PendingItem::Message(input) | PendingItem::ParkedMessage(input) => &input.text,
            PendingItem::Command(t) => t,
        }
    }

    /// Whether this item has a live server-side copy (InjectUser'd to the
    /// server runner queue), so recall/delete must ship a wire QueueRemove to
    /// keep the copy in sync. A ParkedMessage has no copy; a Command is
    /// local-only. Used by recall/delete to decide whether to ship
    /// QueueRemove.
    pub fn is_message(&self) -> bool {
        matches!(self, PendingItem::Message(_))
    }
}

/// Whether a slash command (the text after the leading slash, trimmed) is
/// state-changing -- writes persistent session state or the frame log, so it
/// fights the in-flight run's writes and must be deferred to idle. The
/// narrow set: resume, clear, rewind, undo. Everything else (status, model,
/// search, tips, hooks, debug, trajectory, context, sandbox, cost, graph,
/// diff, agents, memory, help, release-notes, worktrees, export,
/// permissions, stage commands, exit) executes immediately even mid-run --
/// it is UI-local or read-only on the session. exit must stay immediate (a
/// user needs to escape a long run); stage commands are TUI-local (do not
/// touch the server session); export is read-only on the session.
pub fn is_state_changing(stripped: &str) -> bool {
    let cmd = stripped.split_whitespace().next().unwrap_or("");
    matches!(cmd, "clear" | "resume" | "rewind" | "undo")
}

/// Whether a slash command's raw text (with the leading slash) has the given
/// first token (e.g. "resume" for "/resume sid"). Compares the first
/// whitespace-separated token after the slash, so "/resume" + "/resume sid"
/// match but "/resumefoo" does not. Used by swap_session to keep /resume
/// Commands (a switch intent valid in the new session) while dropping other
/// state-changing Commands typed in the OLD session.
pub fn command_first_token_is(raw: &str, token: &str) -> bool {
    raw.trim_start()
        .strip_prefix('/')
        .map(|rest| rest.split_whitespace().next().unwrap_or("") == token)
        .unwrap_or(false)
}

impl App {
    /// Remove one queued item and transfer the active runner copy to the next
    /// eligible head. QueueRemove is sent before the successor is injected.
    pub(crate) fn remove_pending_at(&mut self, index: usize) -> Option<PendingItem> {
        if index >= self.pending.len() {
            return None;
        }
        let item = self.pending.remove(index);
        if let PendingItem::Message(input) = &item {
            self.send_cmd(ClientCommand::QueueRemove {
                session_id: self.session_id.clone(),
                id: input.id,
            });
        }
        self.promote_next_pending();
        Some(item)
    }

    /// Promote a parked head into the live-copy slot: swap to Message +
    /// InjectUser. A Message head already holds the copy; a Command head
    /// is a barrier (never promote past it). No-op when idle -- idle_drain
    /// spawns the head as a fresh run instead. One live copy at a time so
    /// an explicit recall races at most one server copy.
    pub(crate) fn promote_next_pending(&mut self) {
        if !self.agent_busy {
            return;
        }
        let Some(input) = self.pending.first_mut().and_then(|slot| match slot {
            PendingItem::ParkedMessage(input) => {
                let input = input.clone();
                *slot = PendingItem::Message(input.clone());
                Some(input)
            }
            _ => None,
        }) else {
            return;
        };
        let session_id = self.session_id.clone();
        self.send_cmd(ClientCommand::InjectUser { session_id, input });
    }

    /// Dispatch a slash command's raw text (with the leading slash) without
    /// echo. Returns true if a known command matched + ran. Used by the
    /// idle drain (a deferred Command) -- the user echo already landed when
    /// the command was first typed + enqueued, so re-echoing on drain would
    /// double-count it. Follows the slash path of submit_input minus the
    /// echo + the fall-through-to-message.
    pub(crate) fn run_slash_text(&mut self, text: &str) -> bool {
        let Some(stripped) = text.strip_prefix('/') else {
            return false;
        };
        if self.run_tui_local_command(stripped.trim()) {
            return true;
        }
        if let Some(cmd) = houyicoder_protocol::frontend::SlashCommand::parse(text) {
            self.run_command(cmd);
            return true;
        }
        false
    }

    /// Demote every queued Message to ParkedMessage. Call this when the
    /// server's injection buffer is invalidated -- any non-final run end
    /// (interrupt, max-turns, verify-failed, handoff, error), a /clear
    /// reset, or a swap -- because a Message still in the host queue has
    /// lost its server copy. Leaving it as a Message would break the
    /// single-copy invariant (a stale-live item the run no longer backs),
    /// and let promote_next_pending promote a different head, stranding the
    /// orphan. The host queue is the single truth source; the server
    /// queue is only the current run's buffer.
    pub(crate) fn demote_pending_to_parked(&mut self) {
        for it in self.pending.iter_mut() {
            if let PendingItem::Message(t) = it {
                *it = PendingItem::ParkedMessage(t.clone());
            }
        }
    }

    /// The one-time message when a state-changing command is deferred onto the
    /// queue (busy). Resume keeps its busy-aware switch message; the others
    /// get a uniform "will run when the run finishes" line.
    pub(crate) fn deferred_command_message(&self, stripped: &str) -> String {
        let mut parts = stripped.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        if cmd == "resume" {
            let label = parts.next().unwrap_or("");
            return self.resume_switch_message(label);
        }
        format!("{cmd}: will run when the run finishes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_changing_set_narrow() {
        for c in [
            "clear",
            "resume",
            "resume sid",
            "rewind",
            "rewind plan",
            "undo",
        ] {
            assert!(is_state_changing(c), "{c:?} should be state-changing");
        }
    }

    #[test]
    fn test_ui_commands_not_deferred() {
        for c in [
            "status",
            "model",
            "search foo",
            "tips",
            "hooks",
            "debug",
            "trajectory",
            "context",
            "sandbox",
            "cost",
            "graph",
            "diff",
            "agents",
            "memory",
            "help",
            "release-notes",
            "worktrees",
            "export",
            "permissions",
            "init",
            "plan",
            "exit",
        ] {
            assert!(!is_state_changing(c), "{c:?} should execute immediately");
        }
    }

    #[test]
    fn test_command_display_keeps_slash() {
        let c = PendingItem::Command("/resume sid-123".into());
        assert_eq!(c.display(), "/resume sid-123");
        assert!(!c.is_message());
    }

    #[test]
    fn test_message_is_message() {
        let m = PendingItem::Message("hello".into());
        assert!(m.is_message());
        assert_eq!(m.display(), "hello");
    }
}
