//! The /agents pane's row order.
//!
//! One list, in screen order, that both the renderer and the key handlers
//! resolve through, so the row the user sees highlighted is the row an action
//! reaches. Section headings are derived from the row's own section, so a
//! heading can never label a row it does not precede.

use houyicoder_protocol::frontend::agents::AgentEntry;

use crate::agent_message::{DelegationRow, FleetEntry};
use crate::state::App;

/// How many returned delegations the pane keeps. A session can return more
/// than a pane can show; the newest are the ones the user is about to act on.
pub(crate) const COMPLETED_LIMIT: usize = 10;

/// One selectable row. Section headings are not rows: they carry no action,
/// so the cursor never lands on one.
pub(crate) enum AgentRow<'a> {
    /// A child still running.
    Running(&'a FleetEntry),
    /// A delegation that returned this session.
    Completed(&'a DelegationRow),
    /// A type the model may delegate to.
    Available(&'a AgentEntry),
}

impl AgentRow<'_> {
    /// The heading this row is listed under.
    pub(crate) fn section(&self) -> &'static str {
        match self {
            AgentRow::Running(_) => "Running",
            AgentRow::Completed(_) => "Completed",
            AgentRow::Available(_) => "Available types",
        }
    }

    /// The type this row names: the child still running, the delegation that
    /// returned, or the delegatable type itself.
    pub(crate) fn subagent_type(&self) -> &str {
        match self {
            AgentRow::Running(entry) => &entry.subagent_type,
            AgentRow::Completed(done) => &done.subagent_type,
            AgentRow::Available(entry) => &entry.subagent_type,
        }
    }
}

/// The pane's rows in screen order: live children, then the session's returned
/// delegations newest first, then the registered types. A child that has
/// returned is dropped from the live list: it belongs to COMPLETED, and
/// listing it in both would let one delegation answer two Enter presses.
///
/// The two sources settle independently, so neither one alone decides that a
/// child returned. A child's returned delegation lands with the transcript,
/// while the fleet's own completion mark arrives on a status frame of its own;
/// between the two, the live list still holds a child that the returned list
/// already carries, and only the returned list is consulted for that.
pub(crate) fn agent_rows<'a>(
    fleet: &'a [FleetEntry],
    completed: &'a [DelegationRow],
    available: Option<&'a [AgentEntry]>,
) -> Vec<AgentRow<'a>> {
    let mut rows: Vec<AgentRow<'a>> = fleet
        .iter()
        .filter(|entry| entry.completed.is_none())
        .filter(|entry| {
            !completed
                .iter()
                .any(|done| done.child_sid == entry.agent_id)
        })
        .map(AgentRow::Running)
        .collect();
    let start = completed.len().saturating_sub(COMPLETED_LIMIT);
    rows.extend(completed[start..].iter().rev().map(AgentRow::Completed));
    if let Some(available) = available {
        rows.extend(available.iter().map(AgentRow::Available));
    }
    rows
}

/// The rows for the session the app is showing. Every reader goes through
/// this rather than indexing the three sources itself.
pub(crate) fn pane_rows(app: &App) -> Vec<AgentRow<'_>> {
    agent_rows(
        &app.sessions.current.capabilities.fleet.entries,
        &app.sessions.current.agents.rows,
        app.sessions
            .current
            .capabilities
            .agent_types
            .entries
            .as_deref(),
    )
}

#[cfg(test)]
mod tests;
