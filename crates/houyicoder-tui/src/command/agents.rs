//! Agent pane actions: acting on the row the cursor points at.
//!
//! The row comes from the pane's own order, so the row the user sees
//! highlighted is the row the action reaches. Which action depends on the
//! section the row came from: a child, running or returned, opens in the
//! teammate view; a delegatable type opens its detail.

use crate::state::App;
use crate::view::agents_pane::rows::{AgentRow, pane_rows};

/// What the highlighted row opens. Resolved before any mutation so the row
/// borrow ends with the read.
enum RowTarget {
    /// A child session, and whether its transcript still has to be fetched.
    Child {
        child_sid: String,
        needs_fetch: bool,
    },
    /// A delegatable type's detail.
    Type { subagent_type: String },
}

impl App {
    /// Act on the row the /agents pane cursor points at. A row that is no
    /// longer listed does nothing: the pane shows what the session has, and
    /// the cursor is kept within it by every rebuild.
    pub(crate) fn open_agent_row(&mut self) {
        let target = {
            let rows = pane_rows(self);
            let selected = self
                .ui
                .agents_pane
                .cursor()
                .min(rows.len().saturating_sub(1));
            match rows.get(selected) {
                Some(AgentRow::Running(entry)) => Some(RowTarget::Child {
                    child_sid: entry.agent_id.clone(),
                    // A running child's turns are not in the parent
                    // transcript yet, so its view is fetched on open.
                    needs_fetch: true,
                }),
                Some(AgentRow::Completed(row)) => Some(RowTarget::Child {
                    child_sid: row.child_sid.clone(),
                    needs_fetch: !row.loaded,
                }),
                Some(AgentRow::Available(entry)) => Some(RowTarget::Type {
                    subagent_type: entry.subagent_type.clone(),
                }),
                None => None,
            }
        };
        match target {
            Some(RowTarget::Child {
                child_sid,
                needs_fetch,
            }) => {
                self.enter_teammate_view_for_sid(&child_sid, needs_fetch);
            }
            Some(RowTarget::Type { subagent_type }) => {
                self.ui.agents_pane.open_detail(subagent_type);
            }
            None => {}
        }
    }
}
