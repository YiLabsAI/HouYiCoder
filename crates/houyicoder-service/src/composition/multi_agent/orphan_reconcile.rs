//! Reconciling delegation boundaries a host restart left open.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use houyicoder_api::session::SessionLog;
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_core::agent::multi_agent::record_subagent_return;
use houyicoder_protocol::llm::Usage;

use super::children::ChildEntry;
use super::last_assistant_text;

/// Write the missing return for every spawn whose child is neither returned
/// nor still running.
///
/// A child is orphaned when its spawn has no matching return on the parent
/// log and it is not still running. Pair them by child session id: a spawn
/// with any later return for the same id is settled, whatever order the
/// events arrived in. The return carries the child's last assistant text so
/// the parent can resume the work by id rather than paying for it again.
pub(super) async fn run(
    store: &Arc<dyn SessionLog>,
    children: &Arc<Mutex<HashMap<String, ChildEntry>>>,
    parent_sid: SessionId,
) {
    let mut spawned: Vec<String> = Vec::new();
    let mut returned: HashSet<String> = HashSet::new();
    for entry in store.trajectory_tail(parent_sid) {
        match entry.event {
            SessionEvent::SubagentSpawn {
                ref child_session_id,
                ..
            } => {
                if !spawned.contains(child_session_id) {
                    spawned.push(child_session_id.clone());
                }
            }
            SessionEvent::SubagentReturn {
                ref child_session_id,
                ..
            } => {
                returned.insert(child_session_id.clone());
            }
            _ => {}
        }
    }
    let live: HashSet<String> = children
        .lock()
        .map(|g| {
            g.iter()
                .filter(|(_, entry)| entry.runner.upgrade().is_some())
                .map(|(id, _)| id.clone())
                .collect()
        })
        .unwrap_or_default();
    for child in spawned {
        if returned.contains(&child) || live.contains(&child) {
            continue;
        }
        let Some(child_sid) = SessionId::from_display_string(&child) else {
            continue;
        };
        let partial = last_assistant_text(&store.trajectory_tail(child_sid)).unwrap_or_default();
        let summary = format!("[orphaned by host restart, resumable via {child}]\n{partial}");
        if let Err(error) = record_subagent_return(
            store.as_ref(),
            parent_sid,
            &child,
            "interrupted",
            &summary,
            "",
            &Usage::default(),
        )
        .await
        {
            tracing::warn!(?error, %child, "orphan reconciliation return write failed");
        }
    }
}
