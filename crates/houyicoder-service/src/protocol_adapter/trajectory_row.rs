//! Display fields for the trajectory audit row: the event name and the
//! short hash chain link.

use houyicoder_context::SessionEvent;

pub fn event_name(kind: &SessionEvent) -> &'static str {
    match kind {
        SessionEvent::UserInput { .. } => "user",
        SessionEvent::MidTurnInput { .. } => "user",
        SessionEvent::MetaUser { .. } => "meta",
        SessionEvent::MemoryRecall { .. } => "memory",
        SessionEvent::SkillListing { .. } => "skill_listing",
        SessionEvent::SkillBody { .. } => "skill_body",
        SessionEvent::AssistantMessage { .. } => "assistant",
        SessionEvent::AssistantTextDelta { .. } => "delta",
        SessionEvent::ToolCall { .. } => "tool_call",
        SessionEvent::ToolResult { .. } => "tool_result",
        SessionEvent::Reasoning { .. } => "reasoning",
        SessionEvent::CompactionBoundary { .. } => "boundary",
        SessionEvent::CacheBreak { .. } => "cache_break",
        SessionEvent::Summary { .. } => "summary",
        SessionEvent::PermissionDecision { .. } => "verdict",
        SessionEvent::TurnAborted { .. } => "aborted",
        SessionEvent::TruncationVerdict { .. } => "truncation",
        SessionEvent::WorktreeEnter { .. } => "worktree_enter",
        SessionEvent::WorktreeExit { .. } => "worktree_exit",
        SessionEvent::TurnUsage { .. } => "usage",
        SessionEvent::RunCompleted { .. } => "run_completed",
        SessionEvent::HookSignal { .. } => "hook",
        SessionEvent::TurnStarted { .. } => "turn_start",
        SessionEvent::RewardObservation { .. } => "reward",
        SessionEvent::SubagentSpawn { .. } => "spawn",
        SessionEvent::SubagentReturn { .. } => "return",
        SessionEvent::ChildDelegated { .. } => "delegated",
        SessionEvent::NotificationInjected { .. } => "notify",
        SessionEvent::ModelStepTiming { .. } => "timing",
        SessionEvent::ContextCleared { .. } => "cleared",
        SessionEvent::Unknown => "unknown",
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
mod tests;
