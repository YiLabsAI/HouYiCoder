//! Child status snapshots applied to the fleet footer, the drilled-in view,
//! and the transcript.
//!
//! One snapshot drives three places: the footer strip's row for that child,
//! the teammate view's live refetch when the user is watching it, and the
//! one-row trace the parent timeline keeps once the child has ended. All are
//! reactions to the same status frame, so they are applied together here
//! rather than split across the three owners.

use super::*;

use crate::transcript::frame::{FrontendRow, TranscriptFrame};
use crate::transcript::records::{SubagentFinished, SubagentOutcome, TranscriptLine};

impl App {
    /// Apply one child status snapshot to the fleet footer, the teammate
    /// view's live refetch, and the transcript trace.
    pub(super) fn apply_agent_status(&mut self, status: AgentStatusSnapshot) {
        let AgentStatusSnapshot {
            agent_id,
            subagent_type,
            description,
            turn,
            tokens,
            tool_uses,
            last_activity,
            completed,
        } = status;
        // A running child (no completed status) drives the live
        // refetch below; capture it before the fleet update moves
        // the field.
        let is_running = completed.is_none();
        // Auto-exit the teammate view only when the viewed child is
        // gone or broken (killed/failed). A turn-limit, budget, or
        // normal completion leaves partial output worth reading, so
        // the view stays — the user exits with Shift+Up/Down (Esc
        // only interrupts the viewed child's current turn).
        if self
            .ui
            .teammate_view
            .as_ref()
            .is_some_and(|v| v.child_sid == agent_id)
            && completed
                .as_deref()
                .is_some_and(|s| matches!(s, "killed" | "failed"))
        {
            self.exit_teammate_view();
        }
        let mut finished: Option<SubagentFinished> = None;
        // A terminal word for a child the transcript already records is an
        // echo of an ending the fleet already showed and dropped, not news.
        // Reading the frame log once keeps the echo from opening a fresh
        // completed row in the footer that has no trace of its own to match.
        let traced = completed.is_some() && self.transcript_has_trace(&agent_id);
        {
            let entries = &mut self.sessions.current.capabilities.fleet.entries;
            if let Some(entry) = entries.iter_mut().find(|e| e.agent_id == agent_id) {
                entry.turn = turn;
                entry.tokens = tokens;
                entry.tool_uses = tool_uses;
                entry.last_activity = last_activity;
                // A status that carries no summary does not blank the one
                // already held: the row's only label would disappear mid-run.
                if !description.is_empty() {
                    entry.description = description.clone();
                }
                // Stamp the terminal moment the first time a completion
                // lands so the footer grace window starts then; a later
                // status echoing the same completion does not reset it. The
                // same first landing is the one that leaves a trace in the
                // transcript.
                if let Some(word) = completed.as_deref().filter(|_| entry.completed.is_none()) {
                    entry.completed_at = Some(Instant::now());
                    finished = Some(SubagentFinished {
                        child_sid: agent_id.clone(),
                        subagent_type: entry.subagent_type.clone(),
                        description: entry.description.clone(),
                        outcome: SubagentOutcome::from_status(word),
                        duration_ms: entry.started_at.map(|t| t.elapsed().as_millis() as u64),
                        tool_uses: entry.tool_uses,
                    });
                }
                entry.completed = completed;
            } else if !traced {
                // The first word about a child can already be its last. A
                // child first seen finished was never watched running, so it
                // holds no start moment: its trace row shows no duration
                // rather than a run of zero length.
                let ending = completed.as_deref().map(SubagentOutcome::from_status);
                entries.push(FleetEntry {
                    agent_id: agent_id.clone(),
                    subagent_type: subagent_type.clone(),
                    description: description.clone(),
                    turn,
                    tokens,
                    tool_uses,
                    last_activity,
                    completed_at: ending.map(|_| Instant::now()),
                    started_at: ending.is_none().then(Instant::now),
                    completed,
                });
                if let Some(outcome) = ending {
                    finished = Some(SubagentFinished {
                        child_sid: agent_id.clone(),
                        subagent_type,
                        description,
                        outcome,
                        duration_ms: None,
                        tool_uses,
                    });
                }
            }
        }
        // The scan above already answers this: a trace raised here would be
        // the first frame for this child, so the log held none when the
        // status was applied.
        if !traced && let Some(child) = finished {
            self.push_transcript_line(TranscriptLine::SubagentFinished(child));
        }
        // Live-tracking: when the user is viewing a running child,
        // each turn-advance Progress refetches the child transcript
        // so the drilled-in view streams the child's turns as they
        // land (not a frozen snapshot taken at enter). The turn
        // guard debounces: one fetch per turn, not one per status
        // echo. A completed child stops refetching (the final fetch
        // on enter already holds the full result).
        if is_running
            && let Some(view) = self.ui.teammate_view.as_mut()
            && view.child_sid == agent_id
            && view.last_fetched_turn.is_none_or(|t| turn > t)
        {
            view.last_fetched_turn = Some(turn);
            if let Some(s) = self.sessions.current.connection.as_ref() {
                match s.next_request_id() {
                    Ok(req_id) => {
                        self.enqueue_refresh(ClientCommand::ChildTranscriptQuery {
                            req_id,
                            child_sid: FrontendSessionId(agent_id.clone()),
                        });
                    }
                    Err(_) => self.note_request_id_exhausted(),
                }
            }
        }
    }

    /// Whether the transcript already holds a trace for this child.
    ///
    /// The fleet entry cannot answer this on its own. A completed entry
    /// leaves the fleet once its grace window passes, so a terminal word
    /// arriving after that looks like the first word about a new child and
    /// would leave a second trace for one ending. The frame log is the record
    /// that survives the entry, and it is also what the row is drawn from,
    /// so the row exists exactly when the log holds its frame.
    fn transcript_has_trace(&self, child_sid: &str) -> bool {
        self.sessions
            .current
            .transcript
            .frames()
            .iter()
            .any(|sequenced| match sequenced.frame() {
                TranscriptFrame::Frontend(FrontendRow::SubagentFinished(child)) => {
                    child.child_sid == child_sid
                }
                _ => false,
            })
    }
}
