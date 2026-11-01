//! The event vocabulary a hook subscribes to, and the typed payload each
//! event carries. Both are plain data: an event names when a hook runs, and
//! a payload carries what that moment observed, so a hook reads both without
//! anything else.

use std::path::PathBuf;

use houyicoder_context::{AgentId, CheckpointId};

/// Whether a compaction was triggered manually (the /compact command) or
/// automatically (the pre-flight or overflow handler in the agent loop). The
/// trigger is passed to PreCompact/PostCompact hooks so a hook can behave
/// differently for a user-initiated compact versus an auto one; it rides
/// the payload so a hook sees how the compaction was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactTrigger {
    Manual,
    Auto,
}

impl CompactTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }
}

// Supporting types (stubs: defined here until their owning crates land).

/// A tool result payload surfaced to hooks (stub until the hook pipeline
/// tracks the full tool-result shape). Owns just the output string the hook
/// event carries.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub output: String,
}

/// Identifier for a tracked task in the coordination axis.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskId(pub String);

/// Why a session ended. Appears in the SessionEnd payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEndReason {
    Clear,
    Resume,
    Logout,
    Other(String),
}

// HookEvent — 27 lifecycle events plus the select-phase event.

/// Lifecycle events a hook can subscribe to. 27 lifecycle events plus
/// PreSelect (the context select phase, specific to this harness). Grouped into four classification axes:
/// tool lifecycle, session lifecycle, context/compaction, coordination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    // --- Tool lifecycle ---
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,

    // --- Session lifecycle ---
    SessionStart,
    SessionEnd,
    Setup,
    UserPromptSubmit,
    Stop,
    StopFailure,
    Notification,

    // --- Context / compaction ---
    PreCompact,
    PostCompact,
    PreSelect,
    InstructionsLoaded,
    CwdChanged,
    FileChanged,
    ConfigChange,

    // --- Coordination / auxiliary ---
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

impl HookEvent {
    /// Every event the hook system can subscribe to, in declaration order. The
    /// /hooks visibility command lists these so the user sees the full event
    /// list (not just the ones a config registered). 28 total.
    pub const ALL: [HookEvent; 28] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::Setup,
        HookEvent::UserPromptSubmit,
        HookEvent::Stop,
        HookEvent::StopFailure,
        HookEvent::Notification,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::PreSelect,
        HookEvent::InstructionsLoaded,
        HookEvent::CwdChanged,
        HookEvent::FileChanged,
        HookEvent::ConfigChange,
        HookEvent::SubagentStart,
        HookEvent::SubagentStop,
        HookEvent::PermissionRequest,
        HookEvent::PermissionDenied,
        HookEvent::TeammateIdle,
        HookEvent::TaskCreated,
        HookEvent::TaskCompleted,
        HookEvent::Elicitation,
        HookEvent::ElicitationResult,
        HookEvent::WorktreeCreate,
        HookEvent::WorktreeRemove,
    ];

    /// Whether this event has a live fire point wired. The three
    /// tool-lifecycle events fire in the agent loop; the four reserved
    /// subagent and worktree events fire at the service-layer boundaries
    /// (a foreground or background start, the worktree controller enter and
    /// exit). The rest are declared-only. The /hooks command marks fired
    /// events so the user sees which are live vs reserved.
    pub fn is_fired(self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure
                | HookEvent::SubagentStart
                | HookEvent::SubagentStop
                | HookEvent::WorktreeCreate
                | HookEvent::WorktreeRemove
        )
    }

    /// The display label (nominal-case, no underscore) for the /hooks list.
    pub fn label(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::Setup => "Setup",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
            HookEvent::Notification => "Notification",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::PreSelect => "PreSelect",
            HookEvent::InstructionsLoaded => "InstructionsLoaded",
            HookEvent::CwdChanged => "CwdChanged",
            HookEvent::FileChanged => "FileChanged",
            HookEvent::ConfigChange => "ConfigChange",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::PermissionRequest => "PermissionRequest",
            HookEvent::PermissionDenied => "PermissionDenied",
            HookEvent::TeammateIdle => "TeammateIdle",
            HookEvent::TaskCreated => "TaskCreated",
            HookEvent::TaskCompleted => "TaskCompleted",
            HookEvent::Elicitation => "Elicitation",
            HookEvent::ElicitationResult => "ElicitationResult",
            HookEvent::WorktreeCreate => "WorktreeCreate",
            HookEvent::WorktreeRemove => "WorktreeRemove",
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "Before tool execution",
            HookEvent::PostToolUse => "After tool execution",
            HookEvent::PostToolUseFailure => "After tool execution fails",
            HookEvent::SessionStart => "When a new session is started",
            HookEvent::SessionEnd => "When a session is ending",
            HookEvent::Setup => "Repo setup hooks for init and maintenance",
            HookEvent::UserPromptSubmit => "When the user submits a prompt",
            HookEvent::Stop => "Right before the agent concludes its response",
            HookEvent::StopFailure => "When the turn ends due to an API error",
            HookEvent::Notification => "When notifications are sent",
            HookEvent::PreCompact => "Before conversation compaction",
            HookEvent::PostCompact => "After conversation compaction",
            HookEvent::PreSelect => "Before context selection",
            HookEvent::InstructionsLoaded => "When an instruction file is loaded",
            HookEvent::CwdChanged => "After the working directory changes",
            HookEvent::FileChanged => "When a watched file changes",
            HookEvent::ConfigChange => "When configuration files change",
            HookEvent::SubagentStart => "When a subagent is started",
            HookEvent::SubagentStop => "When a subagent concludes its response",
            HookEvent::PermissionRequest => "When a permission dialog is displayed",
            HookEvent::PermissionDenied => "After a tool call is denied",
            HookEvent::TeammateIdle => "When a teammate is about to go idle",
            HookEvent::TaskCreated => "When a task is being created",
            HookEvent::TaskCompleted => "When a task is being marked as completed",
            HookEvent::Elicitation => "When an MCP server requests user input",
            HookEvent::ElicitationResult => "After a user responds to an elicitation",
            HookEvent::WorktreeCreate => "Create an isolated worktree",
            HookEvent::WorktreeRemove => "Remove a previously created worktree",
        }
    }
}

// HookPayload — per-event typed data.

/// Per-event typed payload. Each HookEvent variant carries its own payload
/// shape; hooks match on the event-plus-payload pair. This is a typed
/// payload, not a flat event array with untyped JSON. Field-level
/// differences are noted inline on each variant (added fields, omitted
/// fields). Added fields are for context engineering; omitted fields land
/// when their consumers do.
///
/// Owned (no lifetime): the dispatch path may outlive the caller's stack
/// frame when a hook exceeds its timeout — the thread is abandoned and
/// leaks. Owned data is safe for that model; borrowed data would dangle.
#[derive(Debug, Clone)]
pub enum HookPayload {
    /// Tool call arguments pass through; backfilled_input is added for
    /// the context view (the model may have seen
    /// substituted arguments, not the raw input).
    PreToolUse {
        tool_name: String,
        input: serde_json::Value,
        backfilled_input: Option<serde_json::Value>,
    },
    PostToolUse {
        tool_name: String,
        input: serde_json::Value,
        result: ToolResult,
    },
    /// tool_use_id, error_type, is_interrupt, and is_timeout are omitted
    /// here; they land when the tool pipeline reports them to hooks (the
    /// runner does not track them yet).
    PostToolUseFailure {
        tool_name: String,
        error: String,
    },
    PreCompact {
        trigger: CompactTrigger,
        pre_compact_event_count: usize,
        pre_compact_token_estimate: usize,
    },
    PostCompact {
        trigger: CompactTrigger,
        checkpoint_id: CheckpointId,
        folded_turns: usize,
        compression_ratio: f64,
        /// The summary text the summarizer produced. Passed so a
        /// PostCompact hook can inspect or log what was summarized; it
        /// rides the payload so a hook sees everything the moment carried.
        compact_summary: String,
    },
    PreSelect {
        current_token_estimate: usize,
    },
    SessionStart {
        resumed: bool,
    },
    SessionEnd {
        reason: SessionEndReason,
    },
    UserPromptSubmit {
        prompt: String,
    },
    Stop {
        turn_count: usize,
    },
    StopFailure {
        error: String,
    },
    Notification {
        message: String,
    },
    SubagentStart {
        agent_id: AgentId,
        agent_type: String,
    },
    /// Carries the terminal status plus the child last assistant text so a
    /// hook inspects the result without reading the transcript, and can
    /// branch on the terminal state (completed / max_turns / killed / failed).
    SubagentStop {
        agent_id: AgentId,
        agent_type: String,
        status: String,
        last_text: Option<String>,
    },
    PermissionRequest {
        tool_name: String,
        action: String,
        resource: String,
    },
    PermissionDenied {
        tool_name: String,
        reason: String,
    },
    ConfigChange {
        changed_keys: Vec<String>,
    },
    FileChanged {
        paths: Vec<PathBuf>,
    },
    CwdChanged {
        new_cwd: PathBuf,
    },
    InstructionsLoaded {
        source: String,
    },
    WorktreeCreate {
        path: PathBuf,
    },
    WorktreeRemove {
        path: PathBuf,
    },
    Elicitation {
        request: serde_json::Value,
    },
    ElicitationResult {
        result: serde_json::Value,
    },
    TaskCreated {
        task_id: TaskId,
    },
    TaskCompleted {
        task_id: TaskId,
    },
    TeammateIdle {
        agent_id: AgentId,
    },
    Setup,
}
