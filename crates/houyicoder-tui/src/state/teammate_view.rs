//! Teammate (child agent) transcript view: enter/exit and the active
//! swap. Opened by Enter on a Subagent fold-group line; closed by
//! Shift+Up/Down. Esc only interrupts the viewed child's current turn.

use super::App;
use crate::records::{TeammateView, TranscriptLine};

impl App {
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
    /// footer pill (Enter on a selected fleet row) where the target comes
    /// from the agent id, not the transcript cursor. Mirrors the cursor
    /// path: copy any already-loaded fold rows for an immediate render, and
    /// fire the on-demand fetch when the child transcript is not local.
    pub(crate) fn enter_teammate_view_for_sid(
        &mut self,
        child_sid: &str,
        needs_fetch: bool,
    ) -> bool {
        let mut view = TeammateView {
            child_sid: child_sid.to_string(),
            ..Default::default()
        };
        let mut fire_fetch = needs_fetch;
        for line in &self.transcript {
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
            && let Some(e) = self.fleet.entries.iter().find(|e| e.agent_id == child_sid)
        {
            view.subagent_type = e.subagent_type.clone();
        }
        self.teammate_view = Some(view);
        self.transcript_scroll = crate::scroll::TranscriptScroll::default();
        self.transcript_scroll.follow_tail = true;
        if fire_fetch && let Some(req_id) = self.mint_request_id() {
            self.send_cmd(crate::run_control::ClientCommand::ChildTranscriptQuery {
                req_id,
                child_sid: houyicoder_protocol::frontend::SessionId(child_sid.to_string()),
            });
        }
        true
    }

    /// Exit the teammate view and return to the parent transcript. Clears
    /// the view id; no memory-management release is needed on the sync path
    /// because the child transcript is fetched on demand from the durable
    /// log, not retained for streaming.
    pub(crate) fn exit_teammate_view(&mut self) {
        self.teammate_view = None;
        self.transcript_scroll.follow_tail = true;
    }

    /// Esc while viewing a teammate only interrupts the viewed child's
    /// current turn; it never exits the view. A running child gets a per-turn
    /// cancel (the drive loop cancels the in-flight model fetch, appends an
    /// interrupt marker, starts the next turn — non-terminal). A non-running
    /// child is a no-op on the run; a transient toast reminds the exit
    /// gesture (shift+↑↓) so a misguessed Esc is not silent. Exit is on
    /// Shift+Up/Down, which ignores the running state, so a child that never
    /// idles (a per-turn cancel does not stop the run) cannot trap the user.
    pub(crate) fn abort_viewed_child_turn(&mut self) {
        let Some(view) = self.teammate_view.as_ref() else {
            return;
        };
        let child_sid = view.child_sid.clone();
        let running = self
            .fleet
            .entries
            .iter()
            .find(|e| e.agent_id == child_sid)
            .map(|e| e.completed.is_none())
            .unwrap_or(false);
        if running {
            self.send_cmd(crate::run_control::ClientCommand::CancelChildTurn { child_sid });
        } else {
            // Idle: Esc is a no-op on the run, so teach the exit gesture
            // instead of leaving the press silent. The banner advertises
            // shift+↑↓, but a misguessed Esc is the moment to remind.
            self.notifications
                .add(crate::notifications::Notification::immediate(
                    "teammate-exit-hint",
                    crate::notifications::NotifKind::Text {
                        text: "shift+↑↓ to exit".to_string(),
                        color: None,
                    },
                    std::time::Duration::from_millis(2000),
                ));
        }
    }
}
