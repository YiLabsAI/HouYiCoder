//! The unified pending queue: typed entries for user messages (copied +
//! InjectUser'd to the server runner queue) and slash commands (local-only,
//! never InjectUser'd -- the model must not read a literal "/resume"). A
//! state-changing command submitted while a run is in flight is enqueued
//! here + drained FIFO at idle so it neither fights the in-flight run's
//! writes nor leapfrogs ahead of it. Of the deferred set, resume and clear
//! invalidate the server injection buffer (a swap or reset discards it);
//! rewind and undo do not -- they are deferred for FIFO, not because they
//! orphan the buffer. Assert only the buffer-invalidation dimension here;
//! whether a command goes on the wire is a brittle side-branch.

/// One queued item. The host pending queue is the single truth source for
/// ordering; the server runner queue is only the current run's injection
/// buffer. A Message holds a live server copy (InjectUser'd, consumed
/// mid-turn via QueueConsumed or drained as a follow-up run); a ParkedMessage
/// has NO server copy -- it was either enqueued behind a barrier (a
/// command ahead, which blocks InjectUser) or orphaned by a
/// copy-invalidating event (any non-final run end -- interrupt,
/// max-turns, verify-failed, handoff, error -- a /clear reset, or a swap
/// clears the server queue, so an InjectUser'd message loses its copy).
/// The barrier treats a parked message ahead as a stop: a new message must not
/// InjectUser past a parked one, or the new message would leapfrog it (server
/// consumes the new one mid-turn while the parked one waits for a follow-up
/// run). A slash command is purely local (drained to local dispatch, never
/// sent to the model).
#[derive(Debug, Clone, PartialEq)]
pub enum PendingItem {
    /// A user message with a live server copy (InjectUser'd to the server
    /// runner queue). Consumed mid-turn via QueueConsumed (removed from the
    /// copy) or drained as a follow-up run (QueueRemove + spawn_run) on a
    /// clean run end (FinalOutput) — the user got their answer, so drain FIFO.
    Message(String),
    /// A user message with NO server copy. Either enqueued behind a
    /// barrier (so InjectUser was skipped) or a former Message whose copy
    /// a non-final run end, /clear, or swap invalidated. Drained as a
    /// follow-up run (spawn_run only -- no QueueRemove, there is no copy
    /// to drop) on a clean run end. Recall/delete send no wire QueueRemove
    /// for it.
    ParkedMessage(String),
    /// A slash command's raw text, including the leading slash (e.g.
    /// "/resume <sid>", "/clear"). Stored verbatim so recall
    /// (pop_queued_to_input) re-fills the input box with the exact
    /// text the user typed, and the drain re-dispatches it. Never InjectUser'd.
    Command(String),
}

impl PendingItem {
    /// The text to show in the Ctrl+G queue panel: the message body, or the
    /// command text (with the slash the user typed).
    pub fn display(&self) -> &str {
        match self {
            PendingItem::Message(t) => t,
            PendingItem::ParkedMessage(t) => t,
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

use crate::state::App;

impl App {
    /// Whether a pending item ahead blocks InjectUser of a newly enqueued
    /// message -- a barrier. (1) A Command ahead: resume/clear invalidate
    /// the server injection buffer (a message InjectUser'd past them would
    /// orphan on a server the command throws away); rewind/undo do not,
    /// but a message past them would be consumed mid-run before the command
    /// drains -- a FIFO leapfrog. (2) A ParkedMessage ahead has no copy,
    /// so a message past it would be consumed mid-turn before the parked
    /// one runs -- a FIFO reversal. A Message (live copy) ahead is not a
    /// barrier: the new message joins the server queue behind it. Lifts
    /// once the blocking item drains.
    pub(crate) fn barrier_active(&self) -> bool {
        self.pending.iter().any(|it| match it {
            PendingItem::Command(_) => true,
            PendingItem::ParkedMessage(_) => true,
            PendingItem::Message(_) => false,
        })
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
    /// lost its server copy. Leaving it as a Message would let a newly
    /// enqueued message InjectUser past it (the barrier only blocks on
    /// Command and ParkedMessage), leapfrogging the orphan. The host queue
    /// is the single truth source; the server queue is only the current
    /// run's buffer.
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
