//! Teammate (child agent) transcript view: enter/exit and the active
//! swap. The view is opened by Enter on a Subagent fold-group line and
//! closed by Esc. While open, active_transcript returns the child's
//! projected turns so the working surface renders them with a banner.
//!
//! The child transcript reuses the same on-demand fetch the inline
//! fold-group fills, so the drilled-in view is isomorphic with the
//! expanded fold, not a simplified list. The teammate view swaps the
//! message list to the child's and shows a header naming the agent plus
//! an esc-return hint. The sync path always views a completed child, so
//! Esc always exits.

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
        let Some((child_sid, needs_fetch)) = self.subagent_target_at_cursor() else {
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

    /// Esc while viewing a teammate: a running child aborts its current
    /// turn (the drive loop cancels the in-flight model fetch, appends an
    /// interrupt marker, starts the next turn — non-terminal); a
    /// completed or non-running child exits the view back to the parent.
    /// The running check reads the fleet entry's completion flag so a
    /// child that finished mid-view still exits cleanly on Esc.
    pub(crate) fn esc_teammate_view_or_abort(&mut self) {
        let running = self
            .teammate_view
            .as_ref()
            .and_then(|v| {
                self.fleet
                    .entries
                    .iter()
                    .find(|e| e.agent_id == v.child_sid)
                    .map(|e| e.completed.is_none())
            })
            .unwrap_or(false);
        if running {
            if let Some(view) = self.teammate_view.as_ref() {
                self.send_cmd(crate::run_control::ClientCommand::CancelChildTurn {
                    child_sid: view.child_sid.clone(),
                });
            }
        } else {
            self.exit_teammate_view();
        }
    }
}
