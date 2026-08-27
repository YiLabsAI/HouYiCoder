//! The TUI's wire-message vocabulary: the AgentMessage channel (driver ->
//! event loop) and the ClientCommand channel (event loop -> driver). Split
//! out of run_control so the driver logic and the message types grow
//! independently. The types reference protocol wire shapes only — no engine
//! event types leak through.

use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::SessionId as WireSessionId;
use houyicoder_protocol::frontend::run::{
    ApprovalDecision, ApprovalRequest, ContentBlock, RunError, RunResult,
};
use houyicoder_protocol::llm::EffortLevel;

use crate::transcript::TranscriptFrame;

/// One row in the agent status footer: a spawned child's latest snapshot.
pub struct FleetEntry {
    pub agent_id: String,
    pub subagent_type: String,
    pub turn: u32,
    pub tokens: u64,
    pub tool_uses: u32,
    pub last_activity: Option<String>,
    pub completed: Option<String>,
}

/// The footer fleet state: the child snapshots plus the Shift-arrow
/// selection index. Grouped so the pill reads one field and the
/// struct-fields ratchet counts the pair as one App field, not two.
#[derive(Default)]
pub struct FleetState {
    pub entries: Vec<FleetEntry>,
    pub selected: Option<usize>,
}

/// One message shipped from the client-driver task back to the TUI event
/// loop. The driver is a stateless translator: each durable frame the server
/// ships becomes a Frame message the event loop pushes into its own history
/// (App owns the frame log, not the driver — the
/// frontend owns the message list and the SDK yields deltas). Delta
/// carries streamed text chunks for the live preview. PermissionAsk raises
/// the approval card; Done carries the final outcome. Neither carries a
/// frame snapshot — the event loop's own frame log is the source of truth.
#[derive(Debug)]
pub enum AgentMessage {
    /// One durable wire frame the driver observed. The event loop pushes it
    /// into its own frame log; the transcript projection reads from there.
    Frame(TranscriptFrame),
    /// One incremental chunk of assistant text, emitted as the provider streams.
    Delta { text: String },
    /// One incremental chunk of model reasoning (the breathing thinking row).
    ReasoningDelta { text: String },
    /// A long-running tool (bash) ticks its elapsed seconds so the chip shows
    /// it is not stuck. Routes to app.bash_progress[call_id]; the chip renders
    /// (Ns) after 2s, or (Ns · M lines) when lines is Some. Superseded when
    /// the tool-result frame lands.
    ToolProgress {
        call_id: String,
        elapsed_secs: u64,
        lines: Option<u64>,
    },
    /// Texts the runner drained from its mid-turn injection queue this run.
    /// Each consumed message is removed from the pending-input copy (FIFO +
    /// text match) — a consumed message is no longer pending, so the queue
    /// view + the run-boundary drain stay accurate (no double-spawn at run
    /// end).
    QueueConsumed { texts: Vec<String> },
    /// A runtime notice the agent loop wants surfaced as a system line (e.g.
    /// a provider rejected an over-long request without naming its limit,
    /// pointing the user at the catalog override). The host renders the
    /// pre-rendered text verbatim as a transcript system line.
    SystemLine { text: String },
    /// A mid-turn permission ask the server surfaced as a reverse request. The
    /// TUI raises the approval card; the verdict returns via a ClientCommand
    /// the driver forwards as the matching reverse response. The driver has
    /// already shipped every Frame up to this point, so the event loop's own
    /// frame log is current and the transcript rebuild reads it directly.
    PermissionAsk {
        req_id: RequestId,
        ask: ApprovalRequest,
    },
    /// The run finished (or failed). The driver has already shipped every
    /// Frame for the run, so the event loop rebuilds the transcript from its
    /// own log; no snapshot ships here.
    Done { result: Result<RunResult, RunError> },
    /// A status snapshot the /status command requested over the wire. The
    /// state renders it without importing the engine crate.
    StatusResult {
        snapshot: houyicoder_protocol::frontend::status::StatusSnapshot,
    },
    /// The session trajectory the /trajectory command requested over the wire.
    /// Carries the audit-log entries (3-level drill-down) + the redundant-call
    /// observations (self-evolution reward signal section).
    TrajectoryResult {
        entries: Vec<houyicoder_protocol::frontend::trajectory::TrajectoryEntry>,
        redundant: Vec<houyicoder_protocol::frontend::trajectory::RedundantCallEntry>,
    },
    /// The context-window breakdown the /context command requested over the
    /// wire. The state renders the grid without importing the engine crate.
    ContextResult {
        breakdown: houyicoder_protocol::frontend::context::ContextBreakdown,
    },
    /// The compaction outcome the /compact command requested over the wire.
    /// Carries whether progress was made, the folded event count, the
    /// persisted manifest id, and pre/post token estimates so the state
    /// renders a one-line outcome without importing the engine crate.
    CompactResult {
        reply: houyicoder_protocol::frontend::compact::CompactReply,
    },
    /// The stored-memory list the /memory command requested over the wire.
    /// Frontmatter-only summaries (no body); a /memory <key> show fetches the
    /// body separately.
    MemoryListResult {
        entries: Vec<houyicoder_protocol::frontend::memory::MemorySummaryEntry>,
    },
    /// The full body of one memory the /memory <key> show requested, or None
    /// when the key was absent.
    MemoryShowResult {
        entry: Option<houyicoder_protocol::frontend::memory::MemoryDetail>,
    },
    /// The toggle snapshot the /memory pane requested on open (a read) or the
    /// /memory toggle command requested (a flip). Both auto-memory and
    /// auto-dream ride back so the pane renders both rows from one round-trip.
    MemoryToggleStateResult {
        state: houyicoder_protocol::frontend::memory::ToggleState,
    },
    /// A background memory task wrote the given count of entries this pass.
    /// Fired once per pass on completion (extract: per fork pass plus the
    /// main-agent saved-this-turn skipped path; dream: per consolidation).
    /// The kind tells the renderer the verb (extract = Saved, dream =
    /// Improved).
    MemorySaved {
        count: u32,
        kind: houyicoder_protocol::frontend::memory::MemorySavedKind,
    },
    /// The current permission mode the /model read requested over the wire.
    PermissionModeResult {
        mode: houyicoder_protocol::frontend::permission::PermissionMode,
    },
    /// The durable rule set the /rules read requested over the wire.
    PermissionRulesResult {
        rules: Vec<houyicoder_protocol::frontend::permission::PermissionRule>,
    },
    /// The working directories added to the sandbox at runtime (/permissions
    /// Workspace tab). Refreshes on every add/remove so the tab stays in sync.
    PermissionWorkingDirsResult { dirs: Vec<String> },
    /// The git-confirm checkpoint toggle state the /permission git command requested.
    PermissionAskBeforeGitResult { enabled: bool },
    /// The registered tool list the /tools command requested over the wire.
    ToolListResult {
        tools: Vec<houyicoder_protocol::frontend::tools::ToolEntry>,
    },
    /// The formatted agent directory string the /agents command requested.
    AgentsResult { directory: String },
    /// The on-demand child transcript for an expanded Subagent fold-group.
    /// Frames arrive already converted to TranscriptFrame, so the fill site
    /// runs transcript_from_frames to populate the child rows through the same
    /// projection as the parent flow. child_sid keys the Subagent line to
    /// update in place.
    ChildTranscriptResult {
        child_sid: String,
        frames: Vec<TranscriptFrame>,
    },
    /// The registered hooks the /hooks command requested (read-only visibility).
    HooksResult {
        hooks: Vec<houyicoder_protocol::frontend::hooks::HookEntry>,
    },
    /// The /undo reply: a description of what was undone, or None when the
    /// undo stack was empty.
    UndoResult { description: Option<String> },
    /// The /model select reply: the model id and effort the host actually
    /// applied, so the status bar renders what is being sent rather than what
    /// the picker requested. effort None means no effort parameter is sent.
    ModelResult {
        model: String,
        effort: Option<EffortLevel>,
    },
    /// The /model pane catalog snapshot: the entries to list, the active id,
    /// and the global effort fallback. The pane renders from this rather than
    /// a hardcoded model list, so the rows reflect settings.json.
    ModelInfoResult {
        catalog: houyicoder_protocol::frontend::model::ModelCatalog,
    },
    /// A per-request wire error (a ResponsePayload::Error for a verb that is
    /// NOT a run — a permission/working-dir/mode query the server rejected).
    /// Carries the req_id so the App can tell it apart from a run-failure
    /// (runs surface as Done{Err}); a non-run error becomes a system line,
    /// not a false run-completion (which would corrupt agent_busy mid-run).
    RequestError { req_id: RequestId, message: String },
    /// The /debug reply: whether the diagnostic sink is now recording and
    /// the file path it writes to. Routed to a system line so the user is
    /// told where to look.
    DebugResult {
        state: houyicoder_protocol::frontend::debug::DebugState,
    },
    /// A spawned child's live status snapshot, from the fleet projector.
    /// Drives the agent status footer. completed is None while running.
    AgentStatus {
        agent_id: String,
        subagent_type: String,
        turn: u32,
        tokens: u64,
        tool_uses: u32,
        last_activity: Option<String>,
        completed: Option<String>,
    },
}

/// A command the TUI ships to the client-driver task. The driver owns the
/// protocol Client; the App sends commands over this channel and receives
/// results back as AgentMessage on the agent channel.
pub enum ClientCommand {
    /// Send a MessageSend request (a new user turn). req_id is App-minted.
    SendMessage {
        req_id: RequestId,
        session_id: WireSessionId,
        content: Vec<ContentBlock>,
    },
    /// Answer a pending reverse permission ask with the human verdict.
    Verdict {
        req_id: RequestId,
        decision: ApprovalDecision,
    },
    /// Request a runner status snapshot over the wire (the /status command).
    /// The driver sends the request + ships the reply back as
    /// AgentMessage::StatusResult. req_id is App-minted; distinct from any
    /// active run's req_id so the driver routes the reply correctly.
    StatusQuery {
        req_id: RequestId,
    },
    /// Request the session trajectory over the wire (the /trajectory command).
    TrajectoryQuery {
        req_id: RequestId,
    },
    /// Request a context-window breakdown over the wire (the /context
    /// command).
    ContextQuery {
        req_id: RequestId,
    },
    /// Request a manual compaction over the wire (the /compact command). The
    /// server fires PreCompact hooks, folds older events into a summary,
    /// persists a CheckpointManifest, fires PostCompact, and replies with
    /// the outcome so the host renders a one-line result.
    CompactQuery {
        req_id: RequestId,
    },
    /// Request the current permission mode over the wire (the /model read
    /// path).
    PermissionModeQuery {
        req_id: RequestId,
    },
    /// Request the durable rule set over the wire (the /rules read path).
    PermissionRulesQuery {
        req_id: RequestId,
    },
    /// Request the registered tool list over the wire (the /tools command).
    ToolListQuery {
        req_id: RequestId,
    },
    /// Request the agent directory (registered sub-agent types) for /agents.
    AgentsQuery {
        req_id: RequestId,
    },
    /// Fetch a child agent's transcript on demand, fired on first expand of a
    /// Subagent fold-group with no child rows yet. A re-expand reuses the
    /// cached rows. The server replays the child session log, projects each
    /// turn event through the same session/update + acpx projection the live
    /// push path uses, and returns a one-shot snapshot as ChildTranscriptResult.
    ChildTranscriptQuery {
        req_id: RequestId,
        child_sid: WireSessionId,
    },
    /// Request the registered hooks list over the wire (the /hooks command).
    /// Read-only visibility: which hook events are wired, their name + source.
    HooksQuery {
        req_id: RequestId,
    },
    /// Request the stored-memory list over the wire (the /memory command). The
    /// reply carries frontmatter-only summaries (no body); a /memory <key>
    /// show fetches the body separately.
    MemoryListQuery {
        req_id: RequestId,
    },
    /// Request the full body of one memory by key (the /memory <key> show).
    MemoryShowQuery {
        req_id: RequestId,
        key: String,
    },
    /// Read both memory toggles over the wire (the /memory pane on open). The
    /// reply carries both auto-memory and auto-dream so the pane renders both
    /// rows.
    MemoryToggleStateQuery {
        req_id: RequestId,
    },
    /// Flip one memory toggle and persist (the /memory toggle command). The
    /// reply carries the full snapshot so the pane re-renders both rows.
    MemoryToggleQuery {
        req_id: RequestId,
        which: houyicoder_protocol::frontend::memory::MemoryToggleWhich,
    },
    /// Forget one memory by key + scope (the /memory forget command or the
    /// pane d action). The scope routes the delete to the matching storage
    /// root so forgetting a user/project row deletes the explicit file. The
    /// reply is a refreshed MemoryListResult so the pane narrows.
    MemoryForgetQuery {
        req_id: RequestId,
        key: String,
        scope: String,
    },
    /// Request the /undo over the wire. The server pops the undo stack +
    /// restores the workspace; the reply carries a description of what was
    /// undone (or None when the stack was empty).
    UndoQuery {
        req_id: RequestId,
    },
    PermissionCycleModeQuery {
        req_id: RequestId,
    },
    PermissionAddRuleQuery {
        req_id: RequestId,
        rule: houyicoder_protocol::frontend::permission::PermissionRule,
    },
    PermissionRemoveRuleQuery {
        req_id: RequestId,
        index: usize,
    },
    /// Add a directory the sandboxed agent may touch beyond the workspace
    /// root (/permissions Workspace tab). The server canonicalizes + extends
    /// the kernel fence; the reply carries the updated directory list.
    PermissionAddWorkingDirQuery {
        req_id: RequestId,
        path: String,
    },
    /// Remove a previously-added working directory. No-op when the path was
    /// never added; the reply carries the updated list either way.
    PermissionRemoveWorkingDirQuery {
        req_id: RequestId,
        path: String,
    },
    /// Query or set the git-confirm checkpoint toggle (/permission git [on|off]).
    /// enabled=None queries; Some sets. The reply carries the resulting state.
    PermissionAskBeforeGitQuery {
        req_id: RequestId,
        enabled: Option<bool>,
    },
    /// Query the /model pane catalog (the entries to list + active id +
    /// effort fallback) over the wire. The reply arrives as
    /// ModelInfoResult. Fired when the pane opens so the rows reflect
    /// settings.json, not a hardcoded list.
    ModelInfoQuery {
        req_id: RequestId,
    },
    /// Switch the runner's active model (the /model pane select). The host
    /// resolves a Default sentinel, applies the model id, and persists the
    /// pick; the reply carries the applied ModelApplied. model None means
    /// Default (resolved on the host); effort None means the user left effort
    /// on auto; effort_toggled records whether the user touched effort in the
    /// picker, which the host needs for the persistence rule.
    ModelSwitch {
        req_id: RequestId,
        model: Option<String>,
        effort: Option<EffortLevel>,
        effort_toggled: bool,
    },
    /// Rename the current session (the /status Status tab inline edit). The
    /// server writes the sidecar name + name_source=User (or clears to Auto
    /// on an empty name) and replies with a fresh StatusSnapshot, which the
    /// host routes to StatusResult so the pane + the terminal tab title
    /// refresh together.
    RenameSessionQuery {
        req_id: RequestId,
        session_id: WireSessionId,
        name: String,
    },
    /// Abort the in-flight run (Esc during a run). The driver forwards this
    /// as a session/cancel JSON-RPC notification, which the server's mid-run
    /// select! catches and aborts the runner token; the run resolves
    /// Interrupted and the outcome returns on the original run's req_id. A
    /// notification carries no id (no reply) — the real signal the TUI
    /// watches for is the Done(Interrupted) message. The host sets a
    /// cancelling flag on send and clears it on that Done.
    AbortRun {
        session_id: WireSessionId,
    },
    /// Inject a user message into the in-flight run at the next turn
    /// boundary (the mid-turn interjection path). Sent when the user submits
    /// while a run is busy; the server enqueues it on the runner + the drive
    /// loop drains it at the next turn boundary so the model sees it on its
    /// next call + resumes the current task (not a separate follow-up run).
    /// If the run ends before the next turn boundary, the message stays
    /// queued + the host's run-boundary queue drains it as a follow-up run.
    /// Fire-and-forget notification (no reply). Carries the text since the
    /// host's queue + the frontend pending copy reconcile by text (FIFO).
    InjectUser {
        session_id: WireSessionId,
        text: String,
    },
    /// Steer a running child the user is viewing (teammate view): route the
    /// text into the child's bus inbox rather than starting a parent turn. The
    /// child's drive loop drains it at the next turn boundary. Fire-and-forget.
    InjectToChild {
        child_sid: String,
        text: String,
    },
    /// Remove a queued message by text (the overlay-delete path, or popping
    /// the head to start a follow-up run so the new run does not re-inject
    /// it). The server drops the first queue entry whose text matches;
    /// no-op when it was already drained. Fire-and-forget notification.
    QueueRemove {
        session_id: WireSessionId,
        text: String,
    },
    /// Reset the server's cumulative usage + trajectory for the session (the
    /// /clear command). Fire-and-forget; the ack is ignored — the host
    /// clears its local view in parallel.
    SessionReset {
        req_id: RequestId,
        session_id: WireSessionId,
    },
    /// Toggle the process-wide diagnostic log level (the /debug command).
    /// The server applies the level to every crate that uses tracing and
    /// replies with the resulting state (enabled + path) so the host can
    /// surface where the file is.
    DebugSet {
        req_id: RequestId,
        level: houyicoder_protocol::frontend::debug::DebugLevel,
    },
}
