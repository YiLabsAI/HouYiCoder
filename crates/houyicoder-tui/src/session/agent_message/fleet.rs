//! The fleet view models the footer strip and the agents pane read: one row
//! per started child, one row per returned delegation, and the list built
//! from the transcript. These are presentation state, not protocol messages, so
//! they live apart from the protocol-shaped commands and events.

use std::time::{Duration, Instant};

use crate::records::TranscriptLine;
use crate::transcript::records::SubagentOutcome;

/// One row in the agent status footer: a started child's latest snapshot.
#[derive(Debug)]
pub struct FleetEntry {
    pub agent_id: String,
    pub subagent_type: String,
    /// The delegating call's task summary. A type-only row cannot tell two
    /// children of one type apart, so the strip names the job instead of
    /// repeating the type the user already chose.
    pub description: String,
    pub turn: u32,
    pub tokens: u64,
    pub tool_uses: u32,
    pub last_activity: Option<String>,
    /// How the child ended, once a terminal status named it. None while the
    /// child runs. It is carried as the typed outcome rather than the word
    /// the host sent, so every row that reports the ending switches on one
    /// parsed value; a row that knew only that an ending existed drew it as
    /// done, which told a failed child's story wrong.
    pub outcome: Option<SubagentOutcome>,
    /// When the child reached a terminal state (the ending landed).
    /// Drives the footer auto-background: a completed row stays terse for a
    /// short grace window so the user sees the result landed, then drops
    /// from the footer so the input box rises back under the transcript. The
    /// result stays in the transcript fold-group; only the footer strip leaves.
    pub completed_at: Option<Instant>,
    /// When the child was first observed by the TUI (the FleetEntry was
    /// created from an AgentStatus). Always Some in production; None only in
    /// tests that do not exercise the elapsed counter. Drives the live
    /// elapsed in the strip: build_row renders started_at.elapsed().
    pub started_at: Option<Instant>,
}

/// One returned delegation, for the /agents pane's session list: the child
/// session id to drill into, what it was, and the summary it came back with.
#[derive(Debug, Clone)]
pub struct DelegationRow {
    pub child_sid: String,
    pub subagent_type: String,
    pub summary: String,
    pub prompt: String,
    pub tokens: u64,
    pub tool_uses: u32,
    pub duration_ms: u64,
    /// Whether the child transcript is already loaded in the fold. False on
    /// first open fires the on-demand fetch, same as the inline fold.
    pub loaded: bool,
}

/// The /agents pane's list of this session's returned delegations. Rebuilt
/// from the transcript when the transcript version moves; a returned
/// delegation is durable history, so the list survives the footer strip's
/// grace window (the strip is the present tense, this is the record). The
/// pane's cursor is interaction state and lives on the Ui owner beside its
/// peers; every rebuild, step, and read keeps it within these rows.
#[derive(Debug, Default)]
pub struct PaneAgents {
    pub rows: Vec<DelegationRow>,
    version: u64,
}

impl PaneAgents {
    /// Rebuild the rows when the transcript version moved; a no-op
    /// otherwise, so it can be called on every agent message without cost.
    /// The caller passes the version it read, so the rebuild and the version
    /// check see one consistent transcript.
    pub fn refresh(&mut self, transcript: &[TranscriptLine], version: u64) {
        if version == self.version {
            return;
        }
        self.version = version;
        self.rows = transcript
            .iter()
            .filter_map(|l| match l {
                TranscriptLine::Subagent {
                    child_sid,
                    subagent_type,
                    summary,
                    prompt,
                    folded_transcript,
                    ..
                } => {
                    let tool_uses = folded_transcript
                        .iter()
                        .filter(
                            |l| matches!(l, TranscriptLine::Tool { name, .. } if name != "result"),
                        )
                        .count() as u32;
                    Some(DelegationRow {
                        child_sid: child_sid.clone(),
                        subagent_type: subagent_type.clone(),
                        summary: summary.clone(),
                        prompt: prompt.clone(),
                        tokens: 0,
                        tool_uses,
                        duration_ms: 0,
                        loaded: !folded_transcript.is_empty(),
                    })
                }
                _ => None,
            })
            .collect();
    }
}

/// How long a completed child stays in the footer before dropping. The
/// grace window lets the user see the result landed (a terse done row),
/// then the strip leaves so the input box rises back under the transcript.
/// The result itself stays in the transcript fold-group; only the footer
/// strip drops.
pub const FLEET_GRACE: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests;
