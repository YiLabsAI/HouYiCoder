//! Frontend protocol — message set + slash commands. Stub types; NDJSON
//! framing + capability enforcement to come. See the command spec and the
//! interface spec docs for the contract.

#![allow(dead_code)] // wire types serialized for cross-crate use; locally unused

use serde::{Deserialize, Serialize};

pub mod agents;
pub mod checkpoint;
pub mod compact;
pub mod completion;
pub mod context;
pub mod debug;
pub mod event;
pub mod evidence;
pub mod hooks;
pub mod memory;
pub mod model;
pub mod permission;
pub mod queue;
pub mod recap;
pub mod run;
pub mod session_update;
mod size;
pub mod skills;
pub mod status;
pub mod tasks;
pub mod tools;
pub mod trajectory;
pub mod trust;
pub mod verdict;
pub mod worktree;

use crate::frontend::debug::DebugLevel;
use crate::frontend::model::SpeedMode;
use crate::llm::EffortLevel;
pub use checkpoint::{
    CheckpointImage, CheckpointListView, CheckpointRestoreView, CheckpointSide, CheckpointState,
    CheckpointTextView, CheckpointView, FenceState, RestoreView, SandboxBoundariesView,
    UnreadableView,
};
pub use compact::CompactReply;
pub use completion::CompletionStatus;
pub use context::{
    CategoryBreakdown, ContextBreakdown, ContextFileEntry, ContextSkillEntry, GridCell, build_grid,
    stub_breakdown,
};
pub use event::FrontendEvent;
pub use evidence::{
    AuditEntry, Divergence, Hunk, HunkEvidence, ReviewFinding, SpecClause, audit_entry,
};
pub use memory::{
    MemoryChange, MemoryChangeCausality, MemoryChangeId, MemoryChangeOrigin, MemoryChangeScope,
    MemoryDetail, MemoryOperation, MemorySummaryEntry, MemoryToggleWhich, ToggleState,
};
pub use permission::{
    AllowReason, AskReason, AskSource, DenyReason, DenySource, PermissionDecisionEntry,
    PermissionEffect, PermissionMode, PermissionRule, PermissionRuleContent, RuleDestination,
};
pub use queue::{PendingInputId, QueuedInput};
pub use recap::{FocusReport, RecapNotice};
pub use run::{
    ApprovalDecision, ApprovalRequest, ApprovalScope, ContentBlock, DelegationSource, RunError,
    RunOutcome, RunResult, StopReason,
};
use std::collections::HashSet;
use std::fmt::Display;
use std::fmt::Formatter;
pub use tasks::{
    BackgroundTaskDetail, BackgroundTaskId, BackgroundTaskSnapshot, BackgroundTaskStatus,
};
pub use trajectory::TrajectoryEntry;
pub use trust::{TrustAccept, TrustPrompt, TrustRisk};
pub use verdict::Verdict;
pub use worktree::{WorktreeEnterView, WorktreePreviewView, WorktreeRemoveOutcome};

/// A wire session id, carried by run verbs (MessageSend, RunCancel) so a
/// multi-session host can route a request to the session it drives. A
/// transparent newtype over String so the wire shape is an unadorned string
/// and a typed surface catches a bare String leaking into a session-routed
/// call.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl Display for SessionId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

mod commands;
pub use commands::SlashCommand;

/// Frontend -> daemon requests (NDJSON, capability-gated). One verb per
/// slash command that needs the daemon (local commands like /help are not
/// here). See the command spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FrontendRequest {
    // session and auth
    Login {
        mode: LoginMode,
    },
    Console,
    Archive {
        session_id: String,
    },
    /// Reset the runner's cumulative usage tally + audit trajectory for the
    /// session (the /clear command). The host clears its local view; this
    /// zeros the server-side counters so /status reflects the fresh session.
    SessionReset {
        session_id: SessionId,
    },
    SessionLoad {
        id: String,
    },
    SessionCancel {
        id: String,
    },
    Replay {
        id: String,
    },
    // spec-driven flow
    Init {
        task: String,
    },
    Plan {
        spec_id: String,
    },
    Implement {
        plan_id: String,
    },
    Review {
        target: String,
    },
    Verify {
        target: String,
    },
    // capability
    Spec {
        spec_id: String,
    },
    GraphQuery {
        query: String,
    },
    DiffApply {
        path: String,
        patch: String,
    },
    DiffReject {
        path: String,
    },
    Agents,
    /// Fetch a child agent's transcript on demand (expanding a Subagent
    /// fold-group in the parent flow). The child session id is the agentId
    /// the sync spawn path returned; the reply is a one-shot snapshot of the
    /// child's turn events mapped to the same session/update + acpx frame
    /// stream the parent accumulates. A sync child is terminal by the time
    /// the fold-group renders, so this is a full snapshot, not a live stream.
    ChildTranscript {
        child_sid: SessionId,
    },
    MemoryRecall {
        query: String,
    },
    ModelInfo,
    /// Switch the runner's active model (the /model pane select). The host
    /// resolves a Default sentinel, applies the model id, and persists the
    /// pick; the reply carries the ModelApplyResult so the status bar and
    /// transcript render what is actually being sent. model None means
    /// Default; effort None means the user left effort on auto;
    /// effort_toggled records whether the user touched effort in the picker,
    /// which the host needs to apply the persistence rule (effort equal to
    /// the model default is not persisted). speed None leaves the session
    /// speed tier untouched, so a host that predates Fast mode still
    /// applies the model and effort.
    ModelSet {
        model: Option<String>,
        effort: Option<EffortLevel>,
        effort_toggled: bool,
        #[serde(default)]
        speed: Option<SpeedMode>,
    },
    /// Rename the current session: persist the display name to the session
    /// descriptor (name + name_source=User). An empty name clears back to Auto
    /// (name=None; the display derives the first-prompt slug at render). The
    /// server replies with the applied StatusSnapshot so the host re-renders
    /// /status + the picker reflects the new name on the next list.
    /// Houyi derives Auto names deterministically from the first prompt
    /// (no LLM name-generation round-trip).
    RenameSession {
        session_id: SessionId,
        name: String,
    },
    /// What the host can prove about the fences this session runs inside: the
    /// kernel fence's state, the workspace root, and every directory a write
    /// may land in. Reads host state only, so it is safe while a turn runs.
    SandboxBoundaries,
    /// Every file write the session recorded, with what each target holds now.
    /// Judging a write publishes its committed record when a crash interrupted
    /// the write before it did, so the answer is stable for the next reader.
    SandboxCheckpoints,
    /// Put one recorded write's earlier bytes back. The host re-reads the
    /// target under the same lock the write path takes, so a write this
    /// process ran in between is not undone by mistake. A target something
    /// else changed keeps its current bytes, and the earlier ones go to a copy
    /// beside it.
    SandboxRestore {
        operation: String,
    },
    /// Both sides of one recorded write, as text, for its detail to compare.
    /// The stored images are opened for the row the user opened, so the
    /// listing stays cheap however many writes the session recorded.
    SandboxTexts {
        operation: String,
    },
    // utility
    MessageSend {
        session_id: SessionId,
        content: Vec<crate::frontend::run::ContentBlock>,
        #[serde(default)]
        disabled_skills: HashSet<String>,
    },
    /// Abort the in-flight run. The service cancels the loop, flushes partial
    /// text, and the next outcome returns Interrupted. Fire-and-forget; the
    /// outcome arrives as the response to the original run. Carries the
    /// session id so a multi-session host routes the cancel to the right run.
    RunCancel {
        session_id: SessionId,
        reason: String,
    },
    /// Run one turn that consumes queued completion notifications. The client
    /// sends this only when idle. No user input is written.
    NotificationDrain {
        session_id: SessionId,
        #[serde(default)]
        disabled_skills: HashSet<String>,
    },
    ToolList,
    Hooks,
    McpStatusList,
    McpReconnect {
        name: String,
    },
    McpSetEnabled {
        name: String,
        enabled: bool,
    },
    LspStatusList,
    LspRestart {
        name: String,
    },
    LspSetEnabled {
        name: String,
        enabled: bool,
    },
    Skills,
    /// Fetch the body of one skill by name for the /skills detail view.
    SkillBody {
        name: String,
    },
    ToolCall {
        tool: String,
        args: String,
    },
    Context,
    Status,
    Trajectory,
    /// List every stored memory as a frontmatter-only summary (no body).
    MemoryList,
    /// Fetch the full body of one memory by key.
    MemoryShow {
        key: String,
    },
    /// Read both memory toggles (auto-memory + auto-dream) so the /memory pane
    /// renders the on/off rows from the persisted + in-memory state.
    MemoryToggleState,
    /// Flip one memory toggle and persist the new pair. The response carries
    /// the full snapshot so the pane re-renders both rows from one round-trip.
    MemoryToggle {
        which: crate::frontend::memory::MemoryToggleWhich,
    },
    /// Write the recap switch and its idle window, answering with the
    /// snapshot the Config tab renders from.
    RecapConfigSet {
        enabled: bool,
        idle_secs: u64,
    },
    /// Forget one memory by key (the /memory pane d action or the
    /// /memory forget command). The server deletes the topic + replies with
    /// the refreshed MemoryList so the pane narrows without a second request.
    /// scope is the row's storage scope (user / project / auto) so the
    /// delete routes to the matching root, not just the auto copy.
    MemoryForget {
        key: String,
        scope: String,
    },
    /// Restore one archived memory back into its active scope and reply with
    /// the refreshed MemoryList.
    MemoryRestore {
        key: String,
        scope: String,
    },
    /// Show what removing a managed worktree would discard, plus the token
    /// naming that state. Nothing is written or deleted, so this answer is
    /// safe to ask for while a turn runs.
    ///
    /// The request names the directory the row shows rather than a slug: the
    /// directory is what the terminal has, and the host is the only side that
    /// can say whether it is a worktree this product manages.
    WorktreePreview {
        path: String,
    },
    /// Enter a managed worktree as the session's working tree. The host does
    /// the work itself: no model turn is started, and the reply names the
    /// directory and branch the session now works in.
    WorktreeEnter {
        path: String,
    },
    /// Remove a managed worktree the user confirmed from its preview. The
    /// host re-reads the tree and removes nothing unless the token still
    /// names what it finds.
    WorktreeRemove {
        path: String,
        token: String,
        /// Whether the branch goes with the tree when its commits are not on
        /// the branch it was forked from. False keeps them.
        delete_unmerged_branch: bool,
    },
    Compact,
    Metrics,
    // permission mode and rule set (the TUI renders /mode and /rules without
    // importing the permission crate; the service maps the engine gate
    // state to the wire form at the boundary).
    PermissionMode,
    PermissionRules,
    PermissionCycleMode,
    PermissionAddRule {
        rule: crate::frontend::permission::PermissionRule,
    },
    PermissionRemoveRule {
        index: usize,
    },
    /// Add a directory the sandboxed agent may touch beyond the workspace
    /// root. The server canonicalizes + validates the path is a directory,
    /// extends the kernel fence's allow-back, and responds with the updated
    /// directory list so the Workspace tab stays in sync without a poll. A
    /// path that is not a directory surfaces as a ResponsePayload::Error.
    PermissionAddWorkingDir {
        path: String,
    },
    /// Remove a previously-added working directory. No-op when the path was
    /// never added (or was deleted since); the response carries the updated
    /// list either way.
    PermissionRemoveWorkingDir {
        path: String,
    },
    /// Query or set the git-confirm checkpoint toggle (git commit/rebase/reset/tag Ask
    /// before running). enabled=None queries the current state; Some sets it.
    /// The response always carries the resulting state so the /permission view
    /// stays in sync without a separate poll.
    PermissionAskBeforeGit {
        enabled: Option<bool>,
    },
    // evidence-id coupling (evidence and audit prototype): click-jump and sign-off / replay verbs.
    // All stub.
    SpecClauseJump {
        clause_id: String,
    },
    FindingJump {
        finding_id: String,
    },
    TestJump {
        test_id: String,
    },
    SignOff {
        finding_id: String,
        verdict: Verdict,
    },
    ReplayToPoint {
        event_id: String,
    },
    /// Toggle the process-wide diagnostic log level (the /debug command).
    /// The level applies to every crate that uses the tracing macros, so a
    /// single request turns the engine, the sandbox and the permission gate
    /// on or off together. The response carries the resulting state (enabled
    /// + path) so the host can surface where the file is without guessing.
    DebugSet {
        level: DebugLevel,
    },
    /// Run a command the user typed in the dollar direct-shell mode. The host
    /// executes it in the session sandbox without a model turn, so no LLM is
    /// called. The command is real user input: the host must not let the model
    /// forge one. Capture and result handling reuse the bash path.
    DirectShellRun {
        session_id: SessionId,
        command: String,
    },
    /// Every background command the host knows about, so the tasks pane
    /// renders from one answer instead of assembling state from events.
    ListBackgroundTasks,
    /// One background command's full record, read on demand by the detail
    /// view while the command runs.
    BackgroundTaskDetail {
        id: BackgroundTaskId,
    },
    /// Stop one running background command. The reply acknowledges a kill that
    /// took effect and refuses one that names no running task; the resulting
    /// state arrives as a task event.
    KillBackgroundTask {
        id: BackgroundTaskId,
    },
}

/// Login mode (enterprise SSO, API key, or local/offline).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoginMode {
    Sso,
    ApiKey,
    Local,
}
