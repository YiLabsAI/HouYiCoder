//! Core-to-wire mapping for hook signals: pairs the dispatch outcome with
//! its hook name (HookOutcome) + maps the richer core enums into the
//! leaf-side wire enums the durable HookSignal carries. Split from hook.rs
//! so that file stays under the file-size gate.

use houyicoder_context::{HookErrorKind, HookEventKind, HookVerdictKind};

use super::{HookError, HookEvent, HookVerdict};

/// One hook's outcome from a dispatch, paired with its name so the durable
/// record can attribute each signal to the hook that produced it. The name
/// is NOT inside HookVerdict (only HookError carries it), so dispatch must
/// surface it per result.
#[derive(Debug, Clone)]
pub struct HookOutcome {
    pub hook_name: String,
    pub result: Result<HookVerdict, HookError>,
}

/// The fail-closed policy: a hook that fails to produce a verdict is treated
/// as a denial. Single source of truth — arbitrate drives control flow from
/// this, and the HookSignal recorder writes the same value as the effective
/// verdict, so the durable record can never disagree with what actually
/// happened. If this ever becomes configurable (fail-open), the change lands
/// in one place and neither side can drift.
pub(crate) fn verdict_on_hook_error() -> HookVerdict {
    HookVerdict::Deny("hook error".to_string())
}

/// Map the agent's HookEvent into the wire-level HookEventKind for the
/// durable HookSignal. Exhaustive: if a variant is added to HookEvent
/// without a paired arm, this fails to compile — the maintenance sync is
/// enforced, not left to memory.
pub(crate) fn wire_event_kind(event: HookEvent) -> HookEventKind {
    match event {
        HookEvent::PreToolUse => HookEventKind::PreToolUse,
        HookEvent::PostToolUse => HookEventKind::PostToolUse,
        HookEvent::PostToolUseFailure => HookEventKind::PostToolUseFailure,
        HookEvent::SessionStart => HookEventKind::SessionStart,
        HookEvent::SessionEnd => HookEventKind::SessionEnd,
        HookEvent::Setup => HookEventKind::Setup,
        HookEvent::UserPromptSubmit => HookEventKind::UserPromptSubmit,
        HookEvent::Stop => HookEventKind::Stop,
        HookEvent::StopFailure => HookEventKind::StopFailure,
        HookEvent::Notification => HookEventKind::Notification,
        HookEvent::PreCompact => HookEventKind::PreCompact,
        HookEvent::PostCompact => HookEventKind::PostCompact,
        HookEvent::PreSelect => HookEventKind::PreSelect,
        HookEvent::InstructionsLoaded => HookEventKind::InstructionsLoaded,
        HookEvent::CwdChanged => HookEventKind::CwdChanged,
        HookEvent::FileChanged => HookEventKind::FileChanged,
        HookEvent::ConfigChange => HookEventKind::ConfigChange,
        HookEvent::SubagentStart => HookEventKind::SubagentStart,
        HookEvent::SubagentStop => HookEventKind::SubagentStop,
        HookEvent::PermissionRequest => HookEventKind::PermissionRequest,
        HookEvent::PermissionDenied => HookEventKind::PermissionDenied,
        HookEvent::TeammateIdle => HookEventKind::TeammateIdle,
        HookEvent::TaskCreated => HookEventKind::TaskCreated,
        HookEvent::TaskCompleted => HookEventKind::TaskCompleted,
        HookEvent::Elicitation => HookEventKind::Elicitation,
        HookEvent::ElicitationResult => HookEventKind::ElicitationResult,
        HookEvent::WorktreeCreate => HookEventKind::WorktreeCreate,
        HookEvent::WorktreeRemove => HookEventKind::WorktreeRemove,
    }
}

/// Map a non-error HookVerdict into the wire-level HookVerdictKind. Allow is
/// included for completeness (the recorder skips it before calling, but the
/// mapping is total so the recorder's match stays clean).
pub(crate) fn wire_verdict_kind(verdict: &HookVerdict) -> HookVerdictKind {
    match verdict {
        HookVerdict::Allow => HookVerdictKind::Allow,
        HookVerdict::Deny(_) => HookVerdictKind::Deny,
        HookVerdict::Feedback(_) => HookVerdictKind::Feedback,
        HookVerdict::Observe(_) => HookVerdictKind::Observe,
        HookVerdict::Inject(_) => HookVerdictKind::Inject,
        HookVerdict::Ask(_) => HookVerdictKind::Ask,
        HookVerdict::Trigger(_) => HookVerdictKind::Trigger,
    }
}

/// Map a HookError into the wire-level HookErrorKind (the discriminating kind
/// only; the payload flattens into the HookSignal reason). Exhaustive so a
/// new HookError variant forces a paired arm.
pub(crate) fn wire_error_kind(error: &HookError) -> HookErrorKind {
    match error {
        HookError::GuestPanic { .. } => HookErrorKind::GuestPanic,
        HookError::Timeout { .. } => HookErrorKind::Timeout,
        HookError::InvalidVerdict { .. } => HookErrorKind::InvalidVerdict,
        HookError::CapabilityDenied { .. } => HookErrorKind::CapabilityDenied,
        HookError::FeedbackExhausted { .. } => HookErrorKind::FeedbackExhausted,
        HookError::ConfigError { .. } => HookErrorKind::ConfigError,
        HookError::ProcessError { .. } => HookErrorKind::ProcessError,
    }
}

/// The reason string to record for a hook error (flattened from the
/// structured HookError — the kind is in HookErrorKind, this carries the
/// per-instance detail for human/ExPeL reading).
pub(crate) fn wire_error_reason(error: &HookError) -> String {
    format!("{error:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The compact hooks wire to their HookEventKind variants. Pins the
    /// PreCompact/PostCompress naming.
    #[test]
    fn test_compact_hooks_wire_correctly() {
        assert_eq!(
            wire_event_kind(HookEvent::PreCompact),
            HookEventKind::PreCompact
        );
        assert_eq!(
            wire_event_kind(HookEvent::PostCompact),
            HookEventKind::PostCompact
        );
    }
}
