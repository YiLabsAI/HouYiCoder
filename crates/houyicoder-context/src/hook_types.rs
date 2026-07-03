//! Wire-level hook taxonomy mirrors. The context crate is a serde-only
//! leaf (cannot depend on the agent layer where the richer HookEvent /
//! HookVerdict / HookError live), so the wire types carried by the durable
//! HookSignal event are C-like leaf-side mirrors. The agent loop maps the
//! core enums into these at the append boundary.
//!
//! An exhaustive mapping in core turns "added a core variant but forgot the
//! mirror" into a compile error — mechanism, not memory. Forward-compat
//! (a stale binary reading a newer log variant) is not wired pre-release;
//! it lands as a verify dimension once we ship.

use serde::{Deserialize, Serialize};

/// Wire-level mirror of the hook event taxonomy (tool lifecycle, session
/// lifecycle, context/compaction, coordination). Default is PreToolUse (the
/// most common dispatched event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum HookEventKind {
    #[default]
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    SessionStart,
    SessionEnd,
    Setup,
    UserPromptSubmit,
    Stop,
    StopFailure,
    Notification,
    PreCompact,
    PostCompact,
    PreSelect,
    InstructionsLoaded,
    CwdChanged,
    FileChanged,
    ConfigChange,
    SubagentStart,
    SubagentStop,
    PermissionRequest,
    PermissionDenied,
    TeammateIdle,
    TaskCreated,
    TaskCompleted,
    Elicitation,
    ElicitationResult,
    WorktreeCreate,
    WorktreeRemove,
}

/// Wire-level mirror of the hook verdict kinds. Default is Allow (so an old
/// or partial log deserializes to the no-op verdict).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum HookVerdictKind {
    #[default]
    Allow,
    Deny,
    Feedback,
    Observe,
    Inject,
    Ask,
    Trigger,
}

/// Wire-level mirror of the structured hook error kinds. The payload
/// (hook_name, backtrace, limit_ms, ...) is flattened into the HookSignal
/// reason string — this enum carries only the discriminating kind so ExPeL
/// can aggregate "which failure mode is most common" (Timeout vs GuestPanic
/// vs ...) without regex over a free-text reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HookErrorKind {
    GuestPanic,
    Timeout,
    InvalidVerdict,
    CapabilityDenied,
    FeedbackExhausted,
    ConfigError,
    ProcessError,
}
