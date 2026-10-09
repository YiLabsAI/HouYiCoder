//! Queued user input and deferred session commands.
//!
//! Queue order is local. At most one input is mirrored to the active runner.

use std::ops::Index;

use houyicoder_protocol::frontend::QueuedInput;

use crate::run_control::ClientCommand;
use crate::state::App;

/// One item awaiting commitment or local command execution.
#[derive(Debug, Clone)]
pub enum InputItem {
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

impl PartialEq for InputItem {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Message(a), Self::Message(b))
            | (Self::ParkedMessage(a), Self::ParkedMessage(b)) => a.text == b.text,
            (Self::Command(a), Self::Command(b)) => a == b,
            _ => false,
        }
    }
}

impl InputItem {
    /// The text to show in the queue strip: the message body, or the
    /// command text (with the slash the user typed).
    pub fn display(&self) -> &str {
        match self {
            InputItem::Message(input) | InputItem::ParkedMessage(input) => &input.text,
            InputItem::Command(t) => t,
        }
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
/// match but "/resumefoo" does not. Used by switch_session to keep /resume
/// Commands (a switch intent valid in the new session) while dropping other
/// state-changing Commands typed in the OLD session.
pub fn command_first_token_is(raw: &str, token: &str) -> bool {
    raw.trim_start()
        .strip_prefix('/')
        .map(|rest| rest.split_whitespace().next().unwrap_or("") == token)
        .unwrap_or(false)
}

/// The queued-input FIFO and the selection cursor into it, held as one value.
/// The items stay private so order is a first-in-first-out queue plus an
/// explicit head insert, and the cursor is clamped against the same items it
/// indexes, so the two invariants cannot drift apart.
#[derive(Debug, Default)]
pub(crate) struct InputQueue {
    items: Vec<InputItem>,
    pub(crate) cursor: usize,
}

impl InputQueue {
    /// Append an item to the tail.
    pub(crate) fn push(&mut self, item: InputItem) {
        self.items.push(item);
    }

    /// Insert an item at the given index (re-parking the head after a failed
    /// spawn inserts at index zero).
    pub(crate) fn insert(&mut self, index: usize, item: InputItem) {
        self.items.insert(index, item);
    }

    /// Remove and return the item at the given index.
    pub(crate) fn remove(&mut self, index: usize) -> InputItem {
        self.items.remove(index)
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    // The drain reads the head rather than this predicate; only the
    // queued-input tests assert emptiness, so the library target sees it
    // as unused.
    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(crate) fn first(&self) -> Option<&InputItem> {
        self.items.first()
    }

    pub(crate) fn first_mut(&mut self) -> Option<&mut InputItem> {
        self.items.first_mut()
    }

    pub(crate) fn iter(&self) -> std::slice::Iter<'_, InputItem> {
        self.items.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> std::slice::IterMut<'_, InputItem> {
        self.items.iter_mut()
    }

    /// Take the whole queue out, leaving an empty queue. The cursor is left
    /// as-is, clamped by the same lazy read points as before.
    pub(crate) fn take_all(&mut self) -> Vec<InputItem> {
        std::mem::take(&mut self.items)
    }

    /// Replace the queue contents, leaving the cursor as-is.
    pub(crate) fn replace_all(&mut self, items: Vec<InputItem>) {
        self.items = items;
    }

    /// Clamp the selection cursor to the current queue length.
    pub(crate) fn clamp_cursor(&mut self) {
        self.cursor = self.cursor.min(self.items.len().saturating_sub(1));
    }

    /// Move the selection cursor by a signed delta within the queue bounds.
    pub(crate) fn move_cursor(&mut self, delta: i32) {
        if self.items.is_empty() {
            self.cursor = 0;
            return;
        }
        let cursor = self.cursor.min(self.items.len() - 1) as i32;
        self.cursor = (cursor + delta).clamp(0, (self.items.len() - 1) as i32) as usize;
    }
}

impl Index<usize> for InputQueue {
    type Output = InputItem;

    fn index(&self, index: usize) -> &InputItem {
        &self.items[index]
    }
}

/// Equality against an expected sequence compares only the queued items; the
/// selection cursor is a view position, not queue content, so it is not part
/// of value equality.
impl PartialEq<Vec<InputItem>> for InputQueue {
    fn eq(&self, other: &Vec<InputItem>) -> bool {
        self.items == *other
    }
}

impl App {
    /// Remove one queued item and transfer the active runner copy to the next
    /// eligible head. The server-side QueueRemove is sent before the local
    /// copy is removed so a refused enqueue keeps the item queued and the
    /// local and server mirrors cannot diverge.
    pub(crate) fn remove_pending_at(&mut self, index: usize) -> Option<InputItem> {
        if index >= self.input.queue.len() {
            return None;
        }
        // Enqueue the server removal before touching the local copy: a closed
        // driver leaves the item in place rather than orphaning the mirror.
        if let InputItem::Message(input) = &self.input.queue[index] {
            let id = input.id;
            let session_id = self.session.identity.session_id.clone();
            if let Err(e) = self.enqueue(ClientCommand::QueueRemove { session_id, id }) {
                self.system_line(Self::enqueue_failure_line("queue", e));
                return None;
            }
        }
        let item = self.input.queue.remove(index);
        self.promote_next_pending();
        Some(item)
    }

    /// Promote a parked head into the live-copy slot: swap to Message +
    /// InjectUser. A Message head already holds the copy; a Command head
    /// is a barrier (never promote past it). No-op when idle -- idle_drain
    /// spawns the head as a fresh run instead. The InjectUser is enqueued
    /// before the slot is swapped so a refused enqueue leaves the input
    /// parked, not falsely marked as holding a server mirror.
    pub(crate) fn promote_next_pending(&mut self) {
        if !self.agent_busy() {
            return;
        }
        // Clone the parked input without swapping the slot: the slot only
        // marks itself as a live mirror once the injection is queued.
        let Some(input) = self.input.queue.first().and_then(|slot| match slot {
            InputItem::ParkedMessage(input) => Some(input.clone()),
            _ => None,
        }) else {
            return;
        };
        let session_id = self.session.identity.session_id.clone();
        match self.enqueue(ClientCommand::InjectUser { session_id, input }) {
            Ok(()) => {
                if let Some(slot) = self.input.queue.first_mut()
                    && let InputItem::ParkedMessage(input) = slot
                {
                    let input = input.clone();
                    *slot = InputItem::Message(input);
                }
            }
            Err(e) => {
                self.system_line(Self::enqueue_failure_line("queue", e));
            }
        }
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
        for it in self.input.queue.iter_mut() {
            if let InputItem::Message(t) = it {
                *it = InputItem::ParkedMessage(t.clone());
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
        let c = InputItem::Command("/resume sid-123".into());
        assert_eq!(c.display(), "/resume sid-123");
    }

    #[test]
    fn test_message_display_returns_text() {
        let m = InputItem::Message("hello".into());
        assert_eq!(m.display(), "hello");
    }

    #[test]
    fn test_cursor_clamps_to_bounds() {
        let mut q = InputQueue::default();
        q.move_cursor(1);
        assert_eq!(q.cursor, 0, "an empty queue keeps the cursor at zero");
        q.push(InputItem::Command("/a".into()));
        q.push(InputItem::Command("/b".into()));
        q.cursor = 1;
        q.move_cursor(5);
        assert_eq!(q.cursor, 1, "down movement stops at the last row");
        q.move_cursor(-5);
        assert_eq!(q.cursor, 0, "up movement stops at the first row");
    }
}
