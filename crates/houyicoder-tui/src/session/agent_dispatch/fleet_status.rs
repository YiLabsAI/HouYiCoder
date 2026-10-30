//! Child status snapshots applied to the fleet footer and the drilled-in view.
//!
//! One snapshot reaches two surfaces: the footer strip's row for that child,
//! and the teammate view's live refetch when the user is watching it. Both are
//! reactions to the same wire frame, so they are applied together here rather
//! than split across the two owners.

use super::*;

impl App {
    /// Apply one child status snapshot to the fleet footer and the teammate
    /// view's live refetch.
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
        let entries = &mut self.sessions.current.capabilities.fleet.entries;
        if let Some(entry) = entries.iter_mut().find(|e| e.agent_id == agent_id) {
            entry.turn = turn;
            entry.tokens = tokens;
            entry.tool_uses = tool_uses;
            entry.last_activity = last_activity;
            // A status that carries no summary does not blank the one already
            // held: the row's only label would disappear mid-run.
            if !description.is_empty() {
                entry.description = description.clone();
            }
            // Stamp the terminal moment the first time a completion
            // lands so the footer grace window starts then; a later
            // status echoing the same completion does not reset it.
            if completed.is_some() && entry.completed.is_none() {
                entry.completed_at = Some(Instant::now());
            }
            entry.completed = completed;
        } else {
            entries.push(FleetEntry {
                agent_id: agent_id.clone(),
                subagent_type,
                description,
                turn,
                tokens,
                tool_uses,
                last_activity,
                completed_at: completed.as_ref().map(|_| Instant::now()),
                started_at: Some(Instant::now()),
                completed,
            });
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
}
