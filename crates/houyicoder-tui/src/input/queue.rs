//! Queued user input and deferred session commands.
//!
//! Queue order is local. At most one input is mirrored to the active runner.

use std::ops::Index;

use houyicoder_protocol::frontend::{PendingInputId, QueuedInput};

use crate::agent_message::ClientCommand;
use crate::state::App;
use houyicoder_protocol::frontend::SlashCommand;
use std::mem::take;
use std::slice::Iter;

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
    /// Same kind and same payload. Every pair is listed rather than ending in
    /// a catch-all: a catch-all would let a new variant inherit "never equal",
    /// so a queue comparison would answer false for an item against itself.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Message(a), Self::Message(b))
            | (Self::ParkedMessage(a), Self::ParkedMessage(b)) => a.text == b.text,
            (Self::Command(a), Self::Command(b)) => a == b,
            (
                Self::Message(_) | Self::ParkedMessage(_) | Self::Command(_),
                Self::Message(_) | Self::ParkedMessage(_) | Self::Command(_),
            ) => false,
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

/// The resolved target (sid or export path) of a queued /resume Command, if
/// the item is one. None for any other item or a bare /resume with no arg.
/// The target is the whole remainder after the resume verb, so an export path
/// that contains spaces survives intact rather than being truncated to its
/// first whitespace token. The /resume Command is the carrier for a switch
/// intent across idle ticks and across a loop exit.
pub fn resume_command_target(item: &InputItem) -> Option<&str> {
    let InputItem::Command(text) = item else {
        return None;
    };
    let rest = text.trim_start().strip_prefix('/')?;
    let (verb, tail) = rest.split_once(char::is_whitespace)?;
    if verb != "resume" {
        return None;
    }
    let target = tail.trim();
    if target.is_empty() {
        return None;
    }
    Some(target)
}

/// Whether a slash command's raw text (with the leading slash) has the given
/// first token (e.g. "resume" for "/resume sid"). Compares the first
/// whitespace-separated token after the slash, so "/resume" + "/resume sid"
/// match but "/resumefoo" does not. Drives the session-switch carry rule:
/// /resume Commands cross (a switch intent valid in the new session) while
/// other state-changing Commands typed in the OLD session drop.
pub fn command_first_token_is(raw: &str, token: &str) -> bool {
    raw.trim_start()
        .strip_prefix('/')
        .map(|rest| rest.split_whitespace().next().unwrap_or("") == token)
        .unwrap_or(false)
}

/// The queued-input FIFO and the selection cursor into it, held as one value.
/// The items and the cursor stay private: order is a first-in-first-out queue
/// plus an explicit head insert, and the transitions that shrink the queue
/// clamp the cursor back into range, so the cursor and the items it indexes
/// cannot drift apart.
#[derive(Debug, Default)]
pub(crate) struct InputQueue {
    items: Vec<InputItem>,
    cursor: usize,
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

    /// Remove and return the item at the given index, clamping the cursor so
    /// it stays within the shortened queue.
    pub(crate) fn remove(&mut self, index: usize) -> InputItem {
        let item = self.items.remove(index);
        self.clamp_cursor();
        item
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

    /// The live runner-copy identity held at the given index, when the item
    /// is a Message that mirrors to the runner. None for a parked item (no
    /// runner copy), a Command (never mirrored), or an out-of-range index.
    /// The remove paths read this before dropping the local copy so the
    /// QueueRemove is sent only for an item that actually holds a mirror.
    pub(crate) fn live_copy_at(&self, index: usize) -> Option<PendingInputId> {
        match self.items.get(index) {
            Some(InputItem::Message(input)) => Some(input.id),
            _ => None,
        }
    }

    /// Mark the head as holding a runner copy once its InjectUser was
    /// accepted, so the slot's variant tracks the enqueue outcome rather than
    /// jumping to Message ahead of it. No-op when the head is not a parked
    /// message.
    pub(crate) fn confirm_head_promoted(&mut self) {
        let Some(slot) = self.items.first_mut() else {
            return;
        };
        if let InputItem::ParkedMessage(input) = slot {
            *slot = InputItem::Message(input.clone());
        }
    }

    pub(crate) fn iter(&self) -> Iter<'_, InputItem> {
        self.items.iter()
    }

    /// Take the head if it is a /resume Command with a resolved target,
    /// returning the target and removing the item. A non-/resume head or a
    /// bare /resume (no arg) leaves the queue intact and returns None. The
    /// loop-exit path calls this to hand a pending resume up to the caller
    /// that rebuilds the session outside this loop (no in-process builder).
    pub(crate) fn drain_resume_target(&mut self) -> Option<String> {
        let target = self
            .items
            .first()
            .and_then(resume_command_target)
            .map(str::to_string);
        if target.is_some() {
            self.remove(0);
        }
        target
    }

    /// Take the whole queue out, leaving an empty queue with the cursor reset
    /// to the start.
    pub(crate) fn take_all(&mut self) -> Vec<InputItem> {
        self.cursor = 0;
        take(&mut self.items)
    }

    /// Replace the queue contents, clamping the cursor to the new length so a
    /// shorter replacement keeps the selection in range.
    pub(crate) fn replace_all(&mut self, items: Vec<InputItem>) {
        self.items = items;
        self.clamp_cursor();
    }

    /// Clamp the selection cursor to the current queue length.
    fn clamp_cursor(&mut self) {
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

    /// Reset the selection to the first row. Opening the queue pane from a
    /// collapsed strip or a summary click starts the selection at the top.
    pub(crate) fn select_first(&mut self) {
        self.cursor = 0;
    }

    /// The index the cursor selects, clamped into the queue, or None when the
    /// queue is empty. Callers read the selection through this rather than the
    /// private cursor field.
    pub(crate) fn selected_index(&self) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        Some(self.cursor.min(self.items.len() - 1))
    }

    /// Demote every queued Message to a ParkedMessage. Call this when the
    /// server's injection buffer is invalidated -- any non-final run end
    /// (interrupt, max-turns, verify-failed, handoff, error), a /clear
    /// reset, or a swap -- because a Message still in the host queue has
    /// lost its server copy. Leaving it as a Message would break the
    /// single-copy invariant (a stale-live item the run no longer backs)
    /// and let the next promotion advance a different head, stranding the
    /// orphan. The host queue is the single truth source; the server queue
    /// is only the current run's buffer.
    pub(crate) fn demote_to_parked(&mut self) {
        for it in self.items.iter_mut() {
            if let InputItem::Message(t) = it {
                *it = InputItem::ParkedMessage(t.clone());
            }
        }
    }

    /// Carry the queue across a session switch and return the dropped
    /// command texts for the caller's notice. Session-scoped Commands
    /// (/clear, /rewind, /undo) typed in the old session drop: they act on
    /// whichever session is current when they drain, so carrying one would
    /// apply the old session's intent to the new one. A /resume Command
    /// stays -- the switch intent is still valid in the new session.
    /// Messages stay as user content, demoted to ParkedMessage because the
    /// new runner's server queue is empty: a carried live Message lost its
    /// server copy, and the drain starts a fresh run for it. The selection
    /// resets to the top; a position into the old session's queue is stale.
    pub(crate) fn transfer_across_sessions(&mut self) -> Vec<String> {
        let mut dropped: Vec<String> = Vec::new();
        self.items.retain(|item| {
            if let InputItem::Command(text) = item
                && !command_first_token_is(text, "resume")
            {
                dropped.push(text.clone());
                return false;
            }
            true
        });
        self.demote_to_parked();
        self.cursor = 0;
        dropped
    }

    /// Remove the queued items whose stable identity matches a committed
    /// input, so a delayed commit event cannot remove newer text that happens
    /// to sit at the same position. Membership cleanup only; the caller
    /// promotes the next head afterward.
    pub(crate) fn remove_committed(&mut self, inputs: &[QueuedInput]) {
        for input in inputs {
            if let Some(pos) = self.items.iter().position(|it| {
                matches!(
                    it,
                    InputItem::Message(current) | InputItem::ParkedMessage(current)
                        if current.id == input.id
                )
            }) {
                self.remove(pos);
            }
        }
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
        if index >= self.ui.input.queue.len() {
            return None;
        }
        // Enqueue the server removal before touching the local copy: a closed
        // driver leaves the item in place rather than orphaning the mirror.
        if let Some(id) = self.ui.input.queue.live_copy_at(index) {
            let session_id = self.sessions.current.identity.session_id.clone();
            if let Err(e) = self.enqueue(ClientCommand::QueueRemove { session_id, id }) {
                self.system_line(Self::enqueue_failure_line("queue", e));
                return None;
            }
        }
        let item = self.ui.input.queue.remove(index);
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
        let Some(input) = self.ui.input.queue.first().and_then(|slot| match slot {
            InputItem::ParkedMessage(input) => Some(input.clone()),
            _ => None,
        }) else {
            return;
        };
        let session_id = self.sessions.current.identity.session_id.clone();
        match self.enqueue(ClientCommand::InjectUser { session_id, input }) {
            Ok(()) => self.ui.input.queue.confirm_head_promoted(),
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
        if let Some(cmd) = SlashCommand::parse(text) {
            self.run_command(cmd);
            return true;
        }
        false
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

    /// Consume the pending queue head in first-in, first-out order. Clean
    /// completion may start the next turn; other outcomes leave input parked.
    /// At most one parked message is promoted to the server queue. Returns
    /// false when no item is available or the enqueue was refused.
    pub fn drain_pending_head(&mut self) -> bool {
        let Some(item) = self.ui.input.queue.first().cloned() else {
            return false;
        };
        match item {
            InputItem::Command(text) => {
                self.ui.input.queue.remove(0);
                self.run_slash_text(&text);
                true
            }
            InputItem::Message(head) => {
                // Remove the stale server copy before starting a fresh run; a
                // refused removal keeps the head queued so the local copy and
                // the server mirror cannot diverge.
                let session_id = self.sessions.current.identity.session_id.clone();
                if let Err(e) = self.enqueue(ClientCommand::QueueRemove {
                    session_id,
                    id: head.id,
                }) {
                    self.system_line(Self::enqueue_failure_line("queue", e));
                    return false;
                }
                self.ui.input.queue.remove(0);
                let text = head.text.clone();
                if !self.spawn_run(head.text) {
                    // The mirror is gone but the run could not start: keep the
                    // user content parked at the head so nothing is lost.
                    let queue = &mut self.ui.input.queue;
                    queue.insert(0, InputItem::ParkedMessage(text.into()));
                    return false;
                }
                self.promote_next_pending();
                true
            }
            InputItem::ParkedMessage(input) => {
                self.ui.input.queue.remove(0);
                let text = input.text.clone();
                if !self.spawn_run(input.text) {
                    let queue = &mut self.ui.input.queue;
                    queue.insert(0, InputItem::ParkedMessage(text.into()));
                    return false;
                }
                self.promote_next_pending();
                true
            }
        }
    }

    /// Recall the queued item at the cursor position into the input box.
    /// Sends QueueRemove for a live server copy. Leaves other items queued.
    pub fn recall_queued_at_cursor(&mut self) {
        let Some(idx) = self.ui.input.queue.selected_index() else {
            return;
        };
        let Some(item) = self.remove_pending_at(idx) else {
            return;
        };
        self.ui
            .input
            .merge_recalled_text(item.display().to_string());
    }

    /// Delete the queued item at the cursor position without recalling it.
    /// Sends QueueRemove for a live server copy.
    pub fn delete_queued_at_cursor(&mut self) {
        let Some(idx) = self.ui.input.queue.selected_index() else {
            return;
        };
        drop(self.remove_pending_at(idx));
    }

    /// Recall queued messages into the input box before the current draft.
    /// Commands remain queued. A message leaves the queue only together with
    /// its server mirror: a refused mirror removal keeps the entry queued so
    /// the input cannot duplicate on the next drain.
    pub fn pop_queued_to_input(&mut self) {
        let queue = &self.ui.input.queue;
        if queue.iter().all(|it| matches!(it, InputItem::Command(_))) {
            return;
        }
        let mut keep: Vec<InputItem> = Vec::new();
        let mut recalled: Vec<String> = Vec::new();
        let mut refused = false;
        for it in self.ui.input.queue.take_all() {
            match it {
                InputItem::Message(input) => {
                    let session_id = self.sessions.current.identity.session_id.clone();
                    if self
                        .enqueue(ClientCommand::QueueRemove {
                            session_id,
                            id: input.id,
                        })
                        .is_ok()
                    {
                        recalled.push(input.text);
                    } else {
                        refused = true;
                        keep.push(InputItem::Message(input));
                    }
                }
                InputItem::ParkedMessage(input) => recalled.push(input.text),
                other => keep.push(other),
            }
        }
        self.ui.input.queue.replace_all(keep);
        if refused {
            self.system_line("queue: connection lost — some entries stayed queued");
        }
        if recalled.is_empty() {
            return;
        }
        // Explicit recall supersedes automatic interrupted-turn restoration.
        self.sessions.current.last_run_input = None;
        self.ui.input.merge_recalled_text(recalled.join("\n"));
    }
}

#[cfg(test)]
mod tests;
