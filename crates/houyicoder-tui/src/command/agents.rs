//! Agent pane actions: acting on the row the cursor points at.
//!
//! Pressing Enter on any row opens its L1 detail inside the pane; pressing t
//! (or Enter inside a child's L1 detail) opens the full teammate transcript
//! view.

use crate::state::App;
use crate::view::agents_pane::rows::{AgentRow, pane_rows};

/// What the highlighted row opens. Resolved before any mutation so the row
/// borrow ends with the read.
enum RowTarget {
    /// A child session, and whether its transcript still has to be fetched.
    Child {
        subagent_type: String,
        child_sid: String,
        needs_fetch: bool,
    },
    /// A delegatable type's detail.
    Type { subagent_type: String },
}

impl App {
    fn selected_agent_target(&self) -> Option<RowTarget> {
        let rows = pane_rows(self);
        let selected = self
            .ui
            .agents_pane
            .cursor()
            .min(rows.len().saturating_sub(1));
        match rows.get(selected) {
            Some(AgentRow::Running(entry)) => Some(RowTarget::Child {
                subagent_type: entry.subagent_type.clone(),
                child_sid: entry.agent_id.clone(),
                needs_fetch: true,
            }),
            Some(AgentRow::Completed(row)) => Some(RowTarget::Child {
                subagent_type: row.subagent_type.clone(),
                child_sid: row.child_sid.clone(),
                needs_fetch: !row.loaded,
            }),
            Some(AgentRow::Available(entry)) => Some(RowTarget::Type {
                subagent_type: entry.subagent_type.clone(),
            }),
            None => None,
        }
    }

    /// Open the L1 in-pane detail for the row the /agents pane cursor points at.
    pub(crate) fn open_agent_row(&mut self) {
        match self.selected_agent_target() {
            Some(RowTarget::Child {
                subagent_type,
                child_sid,
                ..
            }) => {
                self.ui
                    .agents_pane
                    .open_child_detail(subagent_type, child_sid);
            }
            Some(RowTarget::Type { subagent_type }) => {
                self.ui.agents_pane.open_detail(subagent_type);
            }
            None => {}
        }
    }

    /// Enter the full teammate transcript view for the open child detail or
    /// the highlighted child row on the list.
    pub(crate) fn open_agent_transcript(&mut self) {
        if let Some(child_sid) = self
            .ui
            .agents_pane
            .detail()
            .and_then(|d| d.child_sid())
            .map(str::to_string)
        {
            let loaded = self
                .sessions
                .current
                .agents
                .rows
                .iter()
                .find(|r| r.child_sid == child_sid)
                .is_some_and(|r| r.loaded);
            self.ui.agents_pane.close_detail();
            self.enter_teammate_view_for_sid(&child_sid, !loaded);
            return;
        }
        if let Some(RowTarget::Child {
            child_sid,
            needs_fetch,
            ..
        }) = self.selected_agent_target()
        {
            self.enter_teammate_view_for_sid(&child_sid, needs_fetch);
        }
    }
}
