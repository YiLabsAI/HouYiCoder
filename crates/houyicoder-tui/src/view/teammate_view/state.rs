//! Teammate (child agent) transcript view: enter/exit and the active
//! swap. Opened by Enter on a Subagent fold-group line; closed by
//! Shift+Up/Down. Esc only interrupts the viewed child's current turn.

use crate::agent_message::FleetEntry;
use crate::records::{TeammateView, TranscriptLine};
use crate::run_control::ClientCommand;
use crate::state::App;
use crate::state::parked_view_states::ParkedViewState;
use houyicoder_protocol::frontend::SessionId;

impl App {
    /// Look up a child's live fleet entry by session id. Shared by the
    /// view-enter fallback, the Esc abort check, and the empty-frames
    /// placeholder so the three never drift apart on the find shape.
    pub(crate) fn fleet_entry(&self, sid: &str) -> Option<&FleetEntry> {
        self.sessions
            .current
            .capabilities
            .fleet
            .entries
            .iter()
            .find(|e| e.agent_id == sid)
    }

    /// Enter the teammate view for the Subagent line at the cursor, or the
    /// most recent Subagent line when no cursor is set. Reuses the cursor
    /// walk shared with toggle_subagent_expand so the line targeted for
    /// inline expand is the line drilled into. When the child transcript is
    /// already loaded into the fold-group, copies it into the view; otherwise
    /// fires the on-demand fetch and the view fills when it returns. Returns
    /// false when no Subagent line exists.
    pub(crate) fn enter_teammate_view(&mut self) -> bool {
        let Some((child_sid, needs_fetch)) = self.subagent_target_or_last() else {
            return false;
        };
        self.enter_teammate_view_for_sid(&child_sid, needs_fetch)
    }

    /// Enter the teammate view for an explicit child session id. Used by the
    /// footer strip (Enter on a selected fleet row) where the target comes
    /// from the agent id, not the transcript cursor. Mirrors the cursor
    /// path: copy any already-loaded fold rows for an immediate render, and
    /// fire the on-demand fetch when the child transcript is not local. A
    /// hop from one child to another passes through the canonical exit
    /// first, so the leaving child parks its own state and the parent's
    /// held sets never mix with a child's.
    pub(crate) fn enter_teammate_view_for_sid(
        &mut self,
        child_sid: &str,
        needs_fetch: bool,
    ) -> bool {
        if self.ui.teammate_view.is_some() {
            self.exit_teammate_view();
        }
        let mut view = TeammateView {
            child_sid: child_sid.to_string(),
            ..Default::default()
        };
        let mut fire_fetch = needs_fetch;
        for line in self.sessions.current.transcript.lines() {
            if let TranscriptLine::Subagent {
                child_sid: sid,
                subagent_type,
                summary: _,
                prompt,
                folded_transcript,
                color,
            } = line
                && sid == child_sid
            {
                view.subagent_type = subagent_type.clone();
                view.prompt = prompt.clone();
                view.color = color.clone();
                if !folded_transcript.is_empty() {
                    view.transcript = folded_transcript.clone();
                    fire_fetch = false;
                }
                break;
            }
        }
        // A running child has no fold-group row yet (the row is created
        // when the result lands). Fall back to the live agent entry for
        // the agent type, which is known at spawn. prompt is not on the
        // live entry (left empty); color is only set by the result frame
        // (left None).
        if view.subagent_type.is_empty()
            && let Some(e) = self.fleet_entry(child_sid)
        {
            view.subagent_type = e.subagent_type.clone();
        }
        // Swap the open-row sets to the child's view: the parent's sets
        // go into the held slot, and what this child parked on its last
        // exit (open rows and viewport) is reinstalled. The parent scroll
        // keeps its own position regardless; a child with nothing parked
        // starts with empty sets at the tail.
        let parent_keys = self.take_expanded_keys();
        if let Some(parked) = self
            .ui
            .parked_view_states
            .enter_child(child_sid, parent_keys)
        {
            view.scroll = parked.scroll;
            self.set_expanded_keys(parked.keys);
        }
        self.ui.teammate_view = Some(view);
        // The render cache keys on transcript_version, which is the
        // invalidation signal for whatever active_transcript returns. Entering
        // the view swaps that source, so bump here or the cache holds the
        // parent rows and the child transcript never renders.
        self.bump_transcript_version();
        if fire_fetch && let Some(s) = self.sessions.current.connection.as_ref() {
            match s.next_request_id() {
                Ok(req_id) => {
                    self.enqueue_refresh(ClientCommand::ChildTranscriptQuery {
                        req_id,
                        child_sid: SessionId(child_sid.to_string()),
                    });
                }
                Err(_) => self.note_request_id_exhausted(),
            }
        }
        true
    }

    /// Exit the teammate view and return to the parent transcript. No
    /// memory-management release is needed on the sync path because the
    /// child transcript is fetched on demand from the durable log, not
    /// retained for streaming.
    pub(crate) fn exit_teammate_view(&mut self) {
        let Some(view) = self.ui.teammate_view.take() else {
            return;
        };
        // Swap the open-row sets back: the child's rows and viewport park
        // under its id for the next visit, and the parent's held sets are
        // reinstalled. The parent scroll stands where the reader left it:
        // entering did not reset it and leaving does not force it to the
        // tail. No trim runs here either — the cap bounds the parent
        // transcript, and the reader never scrolled the parent away from
        // its tail to earn one.
        // A child's viewport lives on the teammate view rather than on the
        // transcript view state, so the two halves are assembled here instead
        // of taken as one value.
        let view_state = ParkedViewState {
            keys: self.take_expanded_keys(),
            scroll: view.scroll,
        };
        if let Some(parent_keys) = self
            .ui
            .parked_view_states
            .exit_child(&view.child_sid, view_state)
        {
            self.set_expanded_keys(parent_keys);
        }
        // Exiting swaps active_transcript back to the parent, so the render
        // cache must drop the child rows and rebuild from the parent.
        self.bump_transcript_version();
    }

    /// Esc while viewing a teammate.
    ///
    /// A running child gets a per-turn cancel: the drive loop cancels the
    /// in-flight model fetch, appends an interrupt marker, and starts the next
    /// turn, which is what the user asked for when they pressed Esc on work in
    /// progress. An idle child has no turn to cancel, so Esc is the way back
    /// out: it returns to wherever the view was entered from, which is the
    /// gesture the user already expects from every other surface. Explaining a
    /// different key instead made leaving a finished child harder than leaving
    /// a running one.
    pub(crate) fn abort_viewed_child_turn(&mut self) {
        let Some(view) = self.ui.teammate_view.as_ref() else {
            return;
        };
        let child_sid = view.child_sid.clone();
        let running = self
            .fleet_entry(&child_sid)
            .map(|e| e.completed.is_none())
            .unwrap_or(false);
        if !running {
            // A draft in the box is a steering message being written, not an
            // empty box the user pressed Esc on. Leaving would throw away the
            // place they were typing into.
            if self.ui.input.editor.is_empty() {
                self.exit_teammate_view();
            }
            return;
        }
        if let Err(e) = self.enqueue(ClientCommand::CancelChildTurn { child_sid }) {
            self.system_line(Self::enqueue_failure_line("cancel", e));
        }
    }
}

#[cfg(test)]
mod tests;
