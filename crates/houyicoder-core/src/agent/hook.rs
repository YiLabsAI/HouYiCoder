//! Hook system: pure-verdict event subscription with host-executed flow control.
//!
//! A hook subscribes to lifecycle events and returns a pure verdict value. The
//! hook trait never executes flow control itself; the host (the agent loop)
//! reads the verdict and decides whether to block, allow, inject, or escalate.
//! This separation keeps hooks deterministic (the same event and payload yield
//! the same verdict, so dispatch is replayable) and safe (a verdict is a value,
//! not a control-flow primitive, so a hook cannot crash the host).
//!
//! Three flow-control modes, all host-side:
//! - Deny gate: a synchronous block on security-sensitive events. The host
//!   short-circuits the tool pipeline and feeds an error result to the model.
//! - Feedback signal: a non-blocking quality signal the host injects into the
//!   next model prompt, enabling in-loop self-correction with bounded retry
//!   and escalation when retries exhaust.
//! - Observe-transform: non-blocking observation or content injection.
//!
//! This crate separates security deny from quality feedback: deny is
//! terminal (no retry), feedback is retryable with a circuit-breaker.
//! Structured errors replace coarse exit-code-plus-stderr signaling so a
//! failing hook never crashes the host.

#![allow(dead_code)] // stub hook types and reserved traits pending owner-crate wiring; locally unused

pub(crate) mod command;
pub(crate) mod pipeline;

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use houyicoder_context::{CheckpointId, SessionId};

use crate::agent::step::AgentId;

/// Whether a compaction was triggered manually (the /compact command) or
/// automatically (the pre-flight or overflow handler in the agent loop). The
/// trigger is passed to PreCompact/PostCompact hooks so a hook can behave
/// differently for a user-initiated compact versus an auto one; it rides
/// the payload so the hook seam stays lossless.
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

// HookOutcome + the core→wire mappings live in the wire submodule (split for
// file-size); verdict_on_hook_error is re-used here by arbitrate.
use self::wire::verdict_on_hook_error;

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

/// Capability token scoping what host functions a hook guest may call.
/// Deny-by-default at the ABI boundary: a guest sees only declared
/// capabilities. Opaque so guest code cannot forge one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityToken {
    capabilities: Vec<String>,
}

impl CapabilityToken {
    pub fn new(caps: Vec<String>) -> Self {
        Self { capabilities: caps }
    }

    pub fn has(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }
}

/// Trust state of a configuration source. Untrusted project hooks are marked
/// skipped (not silently dropped) so the user can see what was elided.
/// Type-enforced: a pre-trust token cannot access trusted actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustState {
    /// User-level or local: trusted by virtue of being on the user's machine.
    Trusted,
    /// Project-level, not yet acknowledged by the user via a trust prompt.
    Untrusted,
    /// User explicitly trusted this project (slash command or flag).
    Acknowledged,
}

/// Why a session ended. Surfaces in the SessionEnd payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEndReason {
    Clear,
    Resume,
    Logout,
    Other(String),
}

/// Policy gating resolved at config-load time (not per-dispatch). Controls
/// which hook sources are active.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HookPolicy {
    /// All sources (user, project, local, managed) are active.
    #[default]
    AllEnabled,
    /// Only managed/policy hooks run.
    ManagedOnly,
    /// Only plugin-registered hooks run.
    PluginOnly,
    /// All hooks disabled.
    Disabled,
}

/// Which configuration level a hook was registered from. The registry
/// uses this to filter hooks by policy at dispatch time. Four levels track
/// the configuration merge hierarchy: user (global config on the user's
/// machine), project (checked into a repository), local (gitignored
/// overrides), and managed (policy-pushed, for example via MDM).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookSource {
    User,
    Project,
    Local,
    Managed,
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
    /// surface (not just the ones a config registered). 28 total.
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

    /// Whether this event has a live fire point wired in the agent loop. Only
    /// the three tool-lifecycle events fire today; the rest are declared-only
    /// (the fire point lands as the feature does). The /hooks command marks
    /// fired events so the user sees which are live vs reserved.
    pub fn is_fired(self) -> bool {
        matches!(
            self,
            HookEvent::PreToolUse | HookEvent::PostToolUse | HookEvent::PostToolUseFailure
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
/// fields). Added fields serve the context-engineering seam; omitted fields
/// land when their consumers do.
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
    /// here; they land when the tool pipeline surfaces them to hooks (the
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
        /// rides the payload so the hook seam stays lossless.
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
    SubagentStop {
        agent_id: AgentId,
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

// HookContext — immutable evaluation context.

/// Immutable context passed to a hook at evaluation time. The hook reads
/// event, payload, and session, then returns a pure verdict. Trust and
/// capability are NOT passed to the hook: trust filtering is centralized
/// in the registry dispatch (the host skips untrusted hooks before
/// evaluation), and capability scoping lands with the WASM executor. This
/// keeps the hook seam minimal and avoids premature fields with no
/// consumers.
///
/// Owned (no lifetime): the parallel dispatch path wraps the context in an
/// Arc and passes it to detached threads. A hook that exceeds its timeout
/// is abandoned — its thread leaks but the context remains valid because
/// the data is owned, not borrowed.
#[derive(Debug, Clone)]
pub struct HookContext {
    pub event: HookEvent,
    pub payload: HookPayload,
    pub session: SessionId,
}

// HookVerdict — the 7 pure verdicts.

/// Pure verdict returned by a hook. The host (agent loop) executes flow
/// control based on this value; the hook trait stays execution-agnostic.
#[derive(Debug, Clone)]
pub enum HookVerdict {
    /// No objection; the host proceeds.
    Allow,
    /// Security hard stop. Synchronous block; terminal (no retry).
    Deny(String),
    /// Quality self-correction signal. The host injects it into the next
    /// model prompt for in-loop rewriting with bounded retry.
    Feedback(String),
    /// Non-blocking observation. The host records it to the observability
    /// log. The string carries the observation note so the host can surface
    /// it without re-deriving context.
    Observe(String),
    /// Inject content into the flowing event (memory recall, redaction,
    /// additional context).
    Inject(String),
    /// Escalate to the user (human-in-the-loop). The host blocks until the
    /// user responds.
    Ask(String),
    /// Fire a downstream event (for example, a large PostToolUse output
    /// triggers a compress). Asynchronous; does not block the current
    /// arbitration.
    Trigger(HookEvent),
}

// HookError — structured, never crashes the host.

/// Structured hook error. All variants are logged to the session store and
/// surfaced via the trajectory view. A hook error never panics the host.
#[derive(Debug, Clone)]
pub enum HookError {
    /// WASM guest trapped (panic or unreachable instruction).
    GuestPanic {
        hook_name: String,
        backtrace: String,
    },
    /// Hook exceeded its time budget. Bounded: default 5s for mechanical
    /// rules, 60s for agent-type verification.
    Timeout { hook_name: String, limit_ms: u64 },
    /// Hook returned an invalid verdict (schema violation).
    InvalidVerdict { hook_name: String, detail: String },
    /// Hook tried to access an undeclared host function.
    CapabilityDenied {
        hook_name: String,
        capability: String,
    },
    /// Feedback retry exhausted (N attempts, still violating the rule).
    FeedbackExhausted { hook_name: String, attempts: u8 },
    /// Malformed hook configuration.
    ConfigError { detail: String },
    /// External-process hook communication failure.
    ProcessError { hook_name: String, reason: String },
}

// ArbitratedVerdict — composite of blocking + non-blocking signals.

// HookOutcome, verdict_on_hook_error, and the core→wire enum mappings live
// in the wire submodule (split for file-size). Re-exported so callers can
// keep naming them through hook::.
pub(crate) mod wire;
pub use wire::HookOutcome;

/// Composite verdict produced by arbitrate. The primary field carries the
/// blocking verdict (the one the host acts on first). Observations and
/// triggers are non-blocking side signals collected from ALL hooks in the
/// batch — they are never dropped even when a blocking verdict is present.
/// This is the core advantage over a single-verdict return: the host can
/// short-circuit on Deny while still recording observations and firing
/// triggers that accompanied the deny.
#[derive(Debug, Clone)]
pub struct ArbitratedVerdict {
    /// The effective blocking verdict, by priority: Deny, Ask, Feedback,
    /// Inject, Trigger (first), Observe (joined), Allow. The host matches
    /// on this for flow control.
    pub primary: HookVerdict,
    /// Non-blocking observation notes. Always collected, even when primary
    /// is Deny/Ask/Feedback/Inject. CONTROL-FLOW RESIDUAL ONLY — the
    /// durable record goes through the dispatch-point HookSignal (one per
    /// hook, with hook_name); this joined Vec is kept for the host's own
    /// use, not for audit. A reader seeing it unconsumed is not seeing a
    /// wiring gap.
    pub observations: Vec<String>,
    /// All trigger events collected from the batch. The host fires these
    /// asynchronously after arbitration. When triggers are the sole signal,
    /// the primary also carries the first trigger for backward compatibility.
    pub triggers: Vec<HookEvent>,
}

// Hook trait — pure-verdict subscription.

/// Pure-verdict hook trait. A hook subscribes to events and returns a
/// verdict; it never executes flow control itself.
///
/// Execution model: WASM in-process (hard-isolated, default) or
/// external-process. The host wraps external-process hooks with an
/// async-to-sync bridge so the trait stays synchronous. Verdicts should be
/// fast (mechanical rules on the hot path, no model calls) and deterministic
/// for replayability.
pub trait Hook: Send + Sync {
    /// Human-readable hook name for diagnostics. Carried into HookError
    /// variants (GuestPanic, Timeout, InvalidVerdict) so the user can see
    /// which hook failed.
    fn name(&self) -> &str;

    /// Which events this hook subscribes to. The registry indexes by these.
    fn events(&self) -> &[HookEvent];

    /// Evaluate the event and payload, return a pure verdict. Must be
    /// deterministic: the same event and payload yield the same verdict.
    /// Errors are structured; never panic.
    fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError>;

    /// Which configuration level this hook was registered from. The
    /// registry uses this to filter hooks by policy at dispatch time.
    fn source(&self) -> HookSource;
}

// HookExecutor — future WASM sandbox seam.

/// Executor for hook logic running inside a WASM sandbox. The host calls
/// the executor, which runs the guest component and translates traps into
/// structured HookError values.
///
/// TODO: implement via Wasmtime Component Model. The target shape is two
/// executors (in-process WASM plus external-process) to drop the SSRF
/// attack surface and keep model-classification off the hot path.
pub trait HookExecutor: Send + Sync {
    /// Run the hook guest for the given context, return its verdict.
    fn execute(&self, ctx: &HookContext) -> Result<HookVerdict, HookError>;
}

// HookRegistry — register, dispatch, collect verdicts.

/// Registry of loaded hooks, multi-level merged and priority-sorted. The
/// host queries hooks by event; hooks return verdicts; the host arbitrates.
/// The registry only collects verdicts; arbitration lives in the host (via
/// the arbitrate helper below) so the registry has a single responsibility.
/// Trust filtering is centralized here at dispatch: when the project trust
/// state is Untrusted, Project and Local source hooks are skipped before
/// evaluation — all hooks gated by one check at entry, not per-hook
/// self-check.
pub(crate) mod registry;
pub(crate) use registry::HookRegistry;

// policy_allows — source vs policy filter predicate.

/// Whether a hook source passes the active policy. ManagedOnly keeps only
/// Managed-source hooks. PluginOnly keeps only Project-source hooks — this
/// is a simplification: the plugin system will introduce a dedicated
/// Plugin source variant, but until it lands, Project is the closest
/// analog (plugins ship with the project). AllEnabled passes everything;
/// Disabled blocks everything (but dispatch short-circuits before reaching
/// here).
fn policy_allows(source: &HookSource, policy: &HookPolicy) -> bool {
    match policy {
        HookPolicy::AllEnabled => true,
        HookPolicy::Disabled => false,
        HookPolicy::ManagedOnly => *source == HookSource::Managed,
        HookPolicy::PluginOnly => *source == HookSource::Project,
    }
}

/// Whether a hook source passes the trust gate. For an Untrusted project,
/// only User and Managed sources pass: User lives in the user's home
/// directory; Managed is policy-pushed (e.g. via MDM). Project and Local
/// sources live in the repository directory — Local is a gitignored
/// override file inside the project, NOT on the user's machine — so a
/// malicious repo could ship a Local hook and run it without trust on an
/// untrusted project. The blanket trust gate: in interactive mode, ALL
/// hooks require trust before they execute.
/// Trusted and Acknowledged projects pass all sources.
fn trust_allows(source: &HookSource, trust: &TrustState) -> bool {
    match source {
        HookSource::User | HookSource::Managed => true,
        HookSource::Project | HookSource::Local => *trust != TrustState::Untrusted,
    }
}

// catch_panic — isolation guard for hook evaluation.

/// Wrap hook evaluation in a panic catch so a panicking hook never crashes
/// the host. A panic is converted to HookError::GuestPanic; the hook_name
/// comes from Hook::name(). True isolation (WASM guest trap) lands with the
/// sandbox executor.
fn catch_panic<F>(f: F, hook_name: &str) -> Result<HookVerdict, HookError>
where
    F: FnOnce() -> Result<HookVerdict, HookError>,
{
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let backtrace = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "non-string panic payload".to_string()
            };
            Err(HookError::GuestPanic {
                hook_name: hook_name.to_string(),
                backtrace,
            })
        }
    }
}

// arbitrate — deny-wins composite arbitration (host-side).

/// Arbitrate a batch of hook results into a composite verdict. The primary
/// field carries the blocking verdict by priority: Deny, Ask, Feedback,
/// Inject, Trigger (first), Observe (joined), Allow. Observations and
/// triggers are ALWAYS collected from every hook regardless of whether a
/// blocking verdict is present — the host short-circuits on Deny while
/// still recording observations + firing triggers that accompanied it.
/// Errors are fail-closed via verdict_on_hook_error (single source).
pub fn arbitrate(
    results: impl IntoIterator<Item = Result<HookVerdict, HookError>>,
) -> ArbitratedVerdict {
    let mut deny: Option<String> = None;
    let mut ask: Option<String> = None;
    let mut feedback: Vec<String> = Vec::new();
    let mut inject: Vec<String> = Vec::new();
    let mut triggers: Vec<HookEvent> = Vec::new();
    let mut observations: Vec<String> = Vec::new();

    for result in results {
        match result {
            Ok(HookVerdict::Allow) => {}
            Ok(HookVerdict::Deny(reason)) => {
                if deny.is_none() {
                    deny = Some(reason);
                }
            }
            Ok(HookVerdict::Feedback(signal)) => feedback.push(signal),
            Ok(HookVerdict::Observe(note)) => observations.push(note),
            Ok(HookVerdict::Inject(content)) => inject.push(content),
            Ok(HookVerdict::Ask(question)) => {
                if ask.is_none() {
                    ask = Some(question);
                }
            }
            Ok(HookVerdict::Trigger(ev)) => triggers.push(ev),
            // Fail-closed: the policy lives in verdict_on_hook_error so the
            // durable HookSignal recorder writes the same effective verdict
            // and the two can never diverge. If this becomes fail-open, one
            // edit changes both sides.
            Err(_) => {
                if let HookVerdict::Deny(reason) = verdict_on_hook_error()
                    && deny.is_none()
                {
                    deny = Some(reason);
                }
            }
        }
    }

    // Primary by priority: deny, ask, feedback, inject, trigger, observe, allow.
    // Observations and triggers are returned alongside regardless of primary.
    let primary = if let Some(reason) = deny {
        HookVerdict::Deny(reason)
    } else if let Some(question) = ask {
        HookVerdict::Ask(question)
    } else if !feedback.is_empty() {
        HookVerdict::Feedback(feedback.join("; "))
    } else if !inject.is_empty() {
        HookVerdict::Inject(inject.join("\n"))
    } else if let Some(first) = triggers.first() {
        HookVerdict::Trigger(*first)
    } else if !observations.is_empty() {
        HookVerdict::Observe(observations.join("; "))
    } else {
        HookVerdict::Allow
    };

    ArbitratedVerdict {
        primary,
        observations,
        triggers,
    }
}

#[cfg(test)]
#[path = "hook_tests.rs"]
mod tests;
