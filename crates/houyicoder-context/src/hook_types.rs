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
use std::path::PathBuf;

use crate::SessionId;

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

/// Leaf payload for the HookFire seam (the service-layer fire point the core
/// HookDispatcher consumes). A flat struct: fields are filled per event, the
/// unused ones stay None. Co-located with HookEventKind because both are
/// hook-taxonomy leaf types the api layer references without depending on
/// core; the payload is a call-time value, not a wire artifact (it never
/// serializes — the durable record is the HookSignal the dispatcher appends).
///
/// Field mapping per fired event:
/// - SubagentStart: agent_id + agent_type.
/// - SubagentStop: agent_id + agent_type + status + last_text. last_text
///   carries the child final assistant text so a hook inspects the result
///   without reading the transcript; status lets a hook branch on the
///   terminal kind (completed / max_turns / killed / failed).
/// - WorktreeCreate / WorktreeRemove: path.
/// - session is always the session the durable HookSignal lands in (the
///   parent session at subagent boundaries, the controller session at
///   worktree boundaries).
#[derive(Debug, Clone)]
pub struct HookFirePayload {
    pub session: SessionId,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub status: Option<String>,
    pub last_text: Option<String>,
    pub path: Option<PathBuf>,
}

impl HookFirePayload {
    /// Payload for a SubagentStart fire: the child agent id + type.
    pub fn subagent_start(session: SessionId, agent_id: String, agent_type: String) -> Self {
        Self {
            session,
            agent_id: Some(agent_id),
            agent_type: Some(agent_type),
            status: None,
            last_text: None,
            path: None,
        }
    }

    /// Payload for a SubagentStop fire: the child agent id + type + terminal
    /// status + the child last assistant text (None when the child emitted
    /// no assistant text).
    pub fn subagent_stop(
        session: SessionId,
        agent_id: String,
        agent_type: String,
        status: String,
        last_text: Option<String>,
    ) -> Self {
        Self {
            session,
            agent_id: Some(agent_id),
            agent_type: Some(agent_type),
            status: Some(status),
            last_text,
            path: None,
        }
    }

    /// Payload for a WorktreeCreate or WorktreeRemove fire: the worktree
    /// path.
    pub fn worktree(session: SessionId, path: PathBuf) -> Self {
        Self {
            session,
            agent_id: None,
            agent_type: None,
            status: None,
            last_text: None,
            path: Some(path),
        }
    }
}
