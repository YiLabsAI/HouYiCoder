//! A short fixed-width label for each event kind, for the trajectory audit
//! row. Split from projection.rs so that file stays under the file-size gate.

use houyicoder_context::TurnEventKind;

pub fn trajectory_kind_label(kind: &TurnEventKind) -> &'static str {
    match kind {
        TurnEventKind::UserInput { .. } => "user",
        TurnEventKind::MidTurnInput { .. } => "user",
        TurnEventKind::MetaUser { .. } => "meta",
        TurnEventKind::MemoryRecall { .. } => "memory",
        TurnEventKind::AssistantMessage { .. } => "assistant",
        TurnEventKind::AssistantTextDelta { .. } => "delta",
        TurnEventKind::ToolCall { .. } => "tool_call",
        TurnEventKind::ToolResult { .. } => "tool_result",
        TurnEventKind::Reasoning { .. } => "reasoning",
        TurnEventKind::CompactionBoundary { .. } => "boundary",
        TurnEventKind::CacheBreak { .. } => "cache_break",
        TurnEventKind::Summary { .. } => "summary",
        TurnEventKind::PermissionDecision { .. } => "verdict",
        TurnEventKind::TurnAborted { .. } => "aborted",
        TurnEventKind::TruncationVerdict { .. } => "truncation",
        TurnEventKind::WorktreeEnter { .. } => "worktree_enter",
        TurnEventKind::WorktreeExit { .. } => "worktree_exit",
        TurnEventKind::TurnUsage { .. } => "usage",
        TurnEventKind::HookSignal { .. } => "hook",
        TurnEventKind::TurnStarted { .. } => "turn_start",
        TurnEventKind::RewardObservation { .. } => "reward",
        TurnEventKind::SubagentSpawn { .. } => "spawn",
        TurnEventKind::SubagentReturn { .. } => "return",
        TurnEventKind::NotificationInjected { .. } => "notify",
        TurnEventKind::Unknown => "unknown",
    }
}

/// The first 8 hex chars of a 32-byte hash, enough to eyeball the chain link.
pub fn hex_short(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(8);
    for b in &bytes[..bytes.len().min(4)] {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{hex_short, trajectory_kind_label};
    use houyicoder_context::TurnEventKind;

    #[test]
    fn test_hex_short_first_bytes() {
        assert_eq!(hex_short(&[0xde, 0xad, 0xbe, 0xef, 0x42]), "deadbeef");
        assert_eq!(hex_short(&[]), "");
        assert_eq!(hex_short(&[0x00, 0xff]), "00ff");
    }

    /// An Unknown kind (a future-binary event type) labels as "unknown" so
    /// the trajectory row does not mislead by borrowing another kind's label.
    #[test]
    fn test_unknown_kind_labeled_unknown() {
        assert_eq!(trajectory_kind_label(&TurnEventKind::Unknown), "unknown");
    }

    #[test]
    fn test_subagent_kinds_labeled() {
        assert_eq!(
            trajectory_kind_label(&TurnEventKind::SubagentSpawn {
                child_session_id: String::new(),
                subagent_type: String::new(),
                prompt_summary: String::new(),
                isolation: String::new(),
                policy: String::new(),
            }),
            "spawn"
        );
        assert_eq!(
            trajectory_kind_label(&TurnEventKind::SubagentReturn {
                child_session_id: String::new(),
                status: String::new(),
                summary: String::new(),
                result_ref: String::new(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: 0,
            }),
            "return"
        );
        assert_eq!(
            trajectory_kind_label(&TurnEventKind::NotificationInjected {
                child_session_id: String::new(),
                turn: 0,
                order: 0,
                topic: String::new(),
            }),
            "notify"
        );
    }
}
