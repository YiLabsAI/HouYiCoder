//! The current session's named capabilities.
//!
//! Each capability owns the state its pane and status paths read, which
//! travels together as one value rather than a scatter of flat App fields.
//! This is the static capability composition from the application model: it
//! holds the server-reported caches plus the client-local session state each
//! capability keeps (such as the disabled-skill set), never a connection
//! handle or a session reader port.

pub(crate) mod model;

use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::env;
use std::time::{Duration, Instant};

use houyicoder_protocol::acp_wire::PermissionOptionKind;
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::agents::AgentEntry;
use houyicoder_protocol::frontend::context::ContextBreakdown;
use houyicoder_protocol::frontend::hooks::HookEntry;
use houyicoder_protocol::frontend::memory::{MemoryChangeId, MemoryToggleWhich, ToggleState};
use houyicoder_protocol::frontend::permission::{
    PermissionDecisionEntry, PermissionMode, PermissionRule,
};
use houyicoder_protocol::frontend::skills::SkillEntry;
use houyicoder_protocol::frontend::status::StatusSnapshot;
use houyicoder_protocol::frontend::tasks::{BackgroundTaskId, BackgroundTaskSnapshot};
use houyicoder_protocol::frontend::tools::ToolEntry;

use crate::agent_message::fleet::{FLEET_GRACE, FleetEntry};
use crate::composition::WorktreeEntry;
use crate::evidence::MemoryEntry;
use crate::session::capabilities::model::ModelCapability;

/// The session's permission state as the server reports it: the current mode,
/// the durable rule list, the allowed-directory list, and the ask-before-git
/// checkpoint, plus the per-tool sticky verdicts, the client-side verdict
/// audit trail, and the frame-log read position that guards the trail. The
/// caches are server-refreshed: the server owns the truth, and the pane
/// renders whatever the last reply shipped.
#[derive(Debug)]
pub(crate) struct PermissionCapability {
    /// The permission mode cache. Seeded on the first idle poll so the status
    /// bar label renders from session start, then replaced by the
    /// PermissionMode / PermissionCycleMode replies. None until seeded.
    pub(crate) mode_cache: Option<PermissionMode>,
    /// Durable rule cache, refreshed by PermissionRulesResult.
    pub(crate) rules_cache: Vec<PermissionRule>,
    /// Allowed-directory cache, refreshed by PermissionDirsResult.
    pub(crate) dirs_cache: Vec<String>,
    /// Ask-before-git checkpoint toggle, refreshed by the permission replies.
    pub(crate) ask_before_git_enabled: bool,
    /// Per-tool last-used verdict keyed by identity, not list position. The
    /// reject cursor preselect reads this so a refused bash lands on No next.
    pub(crate) sticky_choices: HashMap<String, PermissionOptionKind>,
    /// The session verdict log from the permission stream: the client-side
    /// audit trail of every approve or deny.
    pub(crate) verdict_log_cache: Vec<PermissionDecisionEntry>,
    /// The frame-log read position of the verdict audit scan: frames below
    /// this cursor are already folded into verdict_log_cache. A truncation
    /// that rewinds the log past the cursor resets both together so the scan
    /// re-parses from zero against the truncated log.
    pub(crate) verdict_cursor: usize,
    /// The delay before a Level 1 approval card auto-approves in Auto mode, in
    /// seconds. Zero disables the countdown. Read from the
    /// HOUYICODER_AUTO_APPROVE_DELAY_SECS env var so a host or unattended run
    /// can tune or silence it without a settings round-trip.
    pub(crate) auto_approve_delay_secs: u64,
}

impl Default for PermissionCapability {
    fn default() -> Self {
        Self {
            mode_cache: None,
            rules_cache: Vec::new(),
            dirs_cache: Vec::new(),
            ask_before_git_enabled: true,
            sticky_choices: HashMap::new(),
            verdict_log_cache: Vec::new(),
            verdict_cursor: 0,
            auto_approve_delay_secs: auto_approve_delay_secs(),
        }
    }
}

/// The auto-approve countdown delay, read from the environment. Defaults to
/// seven seconds; zero disables the countdown so an Auto session still asks
/// every Level 1 card.
fn auto_approve_delay_secs() -> u64 {
    env::var("HOUYICODER_AUTO_APPROVE_DELAY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7)
}

/// The session's status caches and poll bookkeeping as the wire reports them:
/// the last context breakdown, the latest status snapshot, the stamp of the
/// last periodic status poll, and whether a command-initiated status request
/// is in flight. All of it is wire-refreshed cache or poll bookkeeping; the
/// server owns the truth.
#[derive(Debug, Default)]
pub(crate) struct StatusCapability {
    /// The /context cache: the last breakdown, rendered immediately on a
    /// /context command and refreshed in the background. None until the first
    /// ContextResult.
    pub(crate) context_cache: Option<ContextBreakdown>,
    /// The latest status snapshot. None while disconnected.
    pub(crate) status_cache: Option<StatusSnapshot>,
    /// When the last periodic StatusQuery was sent. Re-fires on the poll
    /// interval so the status bar and the sandbox pane stay recent.
    pub(crate) last_status_poll: Option<Instant>,
    /// Set when a /status command enqueues its query and cleared when the
    /// reply's snapshot is recorded. No production reader is connected yet:
    /// the path that would render the full status block as a transcript line
    /// on a command reply is not built, so the /status pane renders from the
    /// cached snapshot whether or not a command is pending. Kept for that
    /// path to read once it exists; wiring or removing the flag is a separate
    /// change.
    pub(crate) pending_status_command: bool,
}

impl StatusCapability {
    /// The poll cadence for idle status refresh. Each periodic poll updates
    /// the cached snapshot so the status bar and the /status pane stay recent;
    /// it adds no transcript line.
    const POLL_INTERVAL: Duration = Duration::from_secs(1);

    /// The cached status snapshot for the status bar and the /status pane, or
    /// None while disconnected or before the first reply.
    pub(crate) fn snapshot(&self) -> Option<&StatusSnapshot> {
        self.status_cache.as_ref()
    }

    /// Whether the poll interval has elapsed since the last periodic poll, or
    /// no poll has run yet.
    pub(crate) fn poll_due(&self) -> bool {
        self.last_status_poll
            .map(|t| t.elapsed() >= Self::POLL_INTERVAL)
            .unwrap_or(true)
    }

    /// Absorb a status reply: cache the snapshot and clear the pending-command
    /// flag together, so a command poll cannot stay pending past its reply.
    pub(crate) fn record_snapshot(&mut self, snapshot: StatusSnapshot) {
        self.pending_status_command = false;
        self.status_cache = Some(snapshot);
    }

    /// Mark a command-initiated status poll as awaiting its reply. The caller
    /// sets this only after the query enqueue is accepted, so a refused
    /// enqueue never leaves a pending flag waiting on a reply that will not
    /// come.
    pub(crate) fn mark_command_pending(&mut self) {
        self.pending_status_command = true;
    }

    /// Stamp the last periodic poll, throttling the next one by the interval.
    pub(crate) fn mark_poll_sent(&mut self) {
        self.last_status_poll = Some(Instant::now());
    }
}

/// The session's registered-hook cache, the rows the /hooks pane renders.
/// Empty until the first reply; the server owns the truth.
#[derive(Debug, Default)]
pub(crate) struct HooksCapability {
    /// The hook rows in server order.
    pub(crate) entries: Vec<HookEntry>,
}

/// The session's discovered-tool cache, the rows the /tools pane renders.
/// Empty until the first reply; the server owns the truth.
#[derive(Debug, Default)]
pub(crate) struct ToolsCapability {
    /// The tool rows in server order.
    pub(crate) entries: Vec<ToolEntry>,
}

/// The session's discovered-skill cache and disable set: the rows the /skills
/// pane renders plus the names the user disabled this session. The entries
/// are server-refreshed (the server owns the truth); the disable set is
/// client-local session state.
#[derive(Debug, Default)]
pub(crate) struct SkillsCapability {
    /// The skill rows in server order.
    pub(crate) entries: Vec<SkillEntry>,
    /// The skill names the user disabled this session.
    pub(crate) disabled: HashSet<String>,
}

/// The session's linked-worktree listing, the rows the /worktrees pane
/// renders. Filled by the pane-open refresh, which runs a host-side git
/// listing against the session's working directory; empty until then, and
/// reset with the session so rows never ride into a different working
/// directory.
#[derive(Debug, Default)]
pub(crate) struct WorktreeCapability {
    /// The worktree rows in listing order.
    pub(crate) entries: Vec<WorktreeEntry>,
}

/// The session's registered sub-agent types, the rows the /agents pane's
/// available section renders. None until the first reply lands, so the pane
/// can tell "not read yet" from "registered nothing"; the server owns the
/// truth and the same definitions produce the model's prompt paragraph.
#[derive(Debug, Default)]
pub(crate) struct AgentTypesCapability {
    /// The delegatable types in server order (sorted by type).
    pub(crate) entries: Option<Vec<AgentEntry>>,
}

/// The session's fleet facts: one live status snapshot per spawned child,
/// pushed by the host's status relay and updated in place as later snapshots
/// arrive. A completed child drops once its grace window elapses so finished
/// rows do not linger; the footer strip's selection and per-frame drawing
/// facts are interaction state and live on the Ui owner. A switch resets the
/// snapshots by construction: the new session's children are unrelated to the
/// old one's, and a child id can repeat across sessions.
#[derive(Debug, Default)]
pub(crate) struct FleetCapability {
    /// The child snapshots in first-observed order.
    pub(crate) entries: Vec<FleetEntry>,
}

impl FleetCapability {
    /// Whether any child is still running (no ending yet). Drives the
    /// elapsed-counter repaint: only a live child has a counter to update.
    pub(crate) fn any_running(&self) -> bool {
        self.entries.iter().any(|e| e.outcome.is_none())
    }

    /// Drop completed entries whose grace window has elapsed. Running
    /// children (no completed_at) always stay. Returns true when at least
    /// one entry dropped so the caller marks the view dirty.
    ///
    /// retain_viewed pins the child the user is drilled into (teammate
    /// view): its entry stays past the grace window so the footer does not
    /// drop the row the user is reading. The result lives in the transcript
    /// fold-group regardless; this only holds the terse done row. Callers
    /// pass None once the user exits the view.
    pub(crate) fn drop_completed(&mut self, retain_viewed: Option<&str>) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| {
            e.completed_at.is_none_or(|t| t.elapsed() < FLEET_GRACE)
                || e.agent_id == retain_viewed.unwrap_or("")
        });
        self.entries.len() != before
    }

    /// Start the grace window for one child's finished row now.
    ///
    /// The window exists so the user sees the result land before the row
    /// leaves, but the row is pinned while that child is being viewed: a child
    /// that finished a while ago keeps its row only because the reader is
    /// still inside its view. Leaving the view used to drop the row on the
    /// very next frame, because the original completion time was already past
    /// the window. Restamping on exit gives the reader the same five seconds
    /// of goodbye that a child finishing in plain sight gets.
    pub(crate) fn restart_grace(&mut self, agent_id: &str) {
        for entry in &mut self.entries {
            if entry.agent_id == agent_id && entry.outcome.is_some() {
                entry.completed_at = Some(Instant::now());
            }
        }
    }
}

/// An in-flight pane mutation, keyed by the request id its reply will carry.
/// Reads (list refresh, toggle-state fetch) never register an action, so only
/// a mutation's reply writes a transcript outcome.
#[derive(Debug)]
pub(crate) enum MemoryAction {
    Toggle { which: MemoryToggleWhich },
    Forget { key: String },
}

/// The session's memory record: the entry list the server refreshes, the two
/// background-service switches as the server reports them, the change-notice
/// dedupe set, and the in-flight pane mutations keyed by request id. The
/// pane's scope tab, list cursor, search query, and open detail are
/// interaction state and live on the Ui owner beside their peers; this
/// capability holds what the session knows. A switch resets it by
/// construction: the default seeds the bootstrap entries and both switches
/// on, the state a fresh session's pane renders before the first reply.
#[derive(Debug)]
pub(crate) struct MemoryCapability {
    /// The memory rows in server order (newest first). Bootstrap placeholder
    /// rows until the first list reply replaces them.
    pub(crate) entries: Vec<MemoryEntry>,
    toggles: ToggleState,
    seen_changes: HashSet<MemoryChangeId>,
    actions: HashMap<RequestId, MemoryAction>,
}

impl Default for MemoryCapability {
    fn default() -> Self {
        Self {
            entries: bootstrap_entries(),
            toggles: ToggleState {
                auto_memory: true,
                auto_dream: true,
                recap: false,
            },
            seen_changes: HashSet::new(),
            actions: HashMap::new(),
        }
    }
}

/// The bootstrap rows a fresh session's pane shows before the first server
/// list reply lands: one placeholder per storage scope, so the pane is never
/// an empty mystery while the query is in flight.
fn bootstrap_entries() -> Vec<MemoryEntry> {
    vec![
        MemoryEntry {
            topic: "build-gate".to_string(),
            summary: "make check must stay green before commit (placeholder)".to_string(),
            scope: "project".to_string(),
            source: "project".to_string(),
            mtime_secs: 0,
        },
        MemoryEntry {
            topic: "comment-style".to_string(),
            summary: "no CJK, no backtick identifiers in .rs comments (placeholder)".to_string(),
            scope: "user".to_string(),
            source: "feedback".to_string(),
            mtime_secs: 0,
        },
        MemoryEntry {
            topic: "spec-driven".to_string(),
            summary: "every change cites a spec clause and a test id (placeholder)".to_string(),
            scope: "auto".to_string(),
            source: "reference".to_string(),
            mtime_secs: 0,
        },
    ]
}

impl MemoryCapability {
    pub(crate) fn toggles(&self) -> &ToggleState {
        &self.toggles
    }

    pub(crate) fn set_toggles(&mut self, toggles: ToggleState) {
        self.toggles = toggles;
    }

    /// Whether one switch has a flip in flight (the header's pending mark).
    pub(crate) fn toggle_pending(&self, which: MemoryToggleWhich) -> bool {
        self.actions.values().any(
            |action| matches!(action, MemoryAction::Toggle { which: pending } if *pending == which),
        )
    }

    /// Record an in-flight toggle flip. Returns false when the same switch
    /// already has one pending, so a repeat press is dropped instead of
    /// racing on→off→on; the other switch stays operable.
    pub(crate) fn begin_toggle(&mut self, req_id: RequestId, which: MemoryToggleWhich) -> bool {
        if self.toggle_pending(which) {
            return false;
        }
        self.actions.insert(req_id, MemoryAction::Toggle { which });
        true
    }

    /// Record an in-flight forget; the matching list reply writes the
    /// outcome. Returns false when the same key already has one pending, so
    /// a repeat d-press on a row ships a single delete.
    pub(crate) fn begin_forget(&mut self, req_id: RequestId, key: String) -> bool {
        let in_flight = self.actions.values().any(
            |action| matches!(action, MemoryAction::Forget { key: pending } if *pending == key),
        );
        if in_flight {
            return false;
        }
        self.actions.insert(req_id, MemoryAction::Forget { key });
        true
    }

    /// Claim the action a reply's req_id belongs to, clearing it. None for
    /// plain reads and refreshes — they write no transcript.
    pub(crate) fn take_action(&mut self, req_id: RequestId) -> Option<MemoryAction> {
        self.actions.remove(&req_id)
    }

    /// Claim an in-flight toggle flip. None when the id belongs to a forget,
    /// a plain read, or was already claimed — the other action's pending
    /// mark survives a mismatched claim.
    pub(crate) fn take_toggle(&mut self, req_id: RequestId) -> Option<MemoryToggleWhich> {
        match self.actions.get(&req_id) {
            Some(MemoryAction::Toggle { which }) => {
                let which = *which;
                self.actions.remove(&req_id);
                Some(which)
            }
            _ => None,
        }
    }

    /// Claim an in-flight forget. None when the id belongs to a toggle, a
    /// plain read, or was already claimed — the other action's pending mark
    /// survives a mismatched claim.
    pub(crate) fn take_forget(&mut self, req_id: RequestId) -> Option<String> {
        match self.actions.get(&req_id) {
            Some(MemoryAction::Forget { key }) => {
                let key = key.clone();
                self.actions.remove(&req_id);
                Some(key)
            }
            _ => None,
        }
    }

    /// Drop every in-flight mutation mark. Called when the connection dies —
    /// no reply can ever resolve them, and a sticky pending mark would refuse
    /// its switch forever. The pane's loading detail is swept beside this by
    /// the Ui-side half of the same cleanup.
    pub(crate) fn clear_actions(&mut self) {
        self.actions.clear();
    }

    /// The keys with a forget in flight, sorted for deterministic assertions.
    #[cfg(test)]
    pub(crate) fn pending_forget_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .actions
            .values()
            .filter_map(|action| match action {
                MemoryAction::Forget { key } => Some(key.clone()),
                MemoryAction::Toggle { .. } => None,
            })
            .collect();
        keys.sort();
        keys
    }

    /// How many switches have a flip in flight, for repeat-press assertions.
    #[cfg(test)]
    pub(crate) fn pending_toggle_count(&self) -> usize {
        self.actions
            .values()
            .filter(|action| matches!(action, MemoryAction::Toggle { .. }))
            .count()
    }

    pub(crate) fn register_change(&mut self, id: &MemoryChangeId) -> bool {
        self.seen_changes.insert(id.clone())
    }
}

/// The tasks capability: the newest snapshot of each background command, as
/// the host published it. The host owns the commands; this cache answers the
/// status bar's running count, the ending a background call's row finally
/// shows, and the tasks pane's list. A frame replaces the snapshot of the
/// task it names, and the revision moves with every change so the row cache
/// rebuilds against it.
#[derive(Debug, Default)]
pub(crate) struct TasksCapability {
    /// Newest snapshot per task number, ordered by number.
    snapshots: BTreeMap<BackgroundTaskId, BackgroundTaskSnapshot>,
    /// Set once a listing has replaced the cache, so the session knows the
    /// cache speaks for the host rather than for the frames it happened to
    /// see. A row restored from the log reads this before saying its task is
    /// gone: before a listing, a missing task means nothing was observed yet.
    listed: bool,
    revision: u64,
}

impl TasksCapability {
    /// Record the newest snapshot of one task.
    ///
    /// An ending is final: a later frame that still reports the task running
    /// is older than what the cache holds, so it is dropped rather than
    /// reverting a row the user already read as finished.
    pub(crate) fn record(&mut self, task: BackgroundTaskSnapshot) {
        let reverts_ending = self
            .snapshots
            .get(&task.id)
            .is_some_and(|held| held.status.is_terminal() && !task.status.is_terminal());
        if reverts_ending {
            return;
        }
        self.snapshots.insert(task.id, task);
        self.revision = self.revision.wrapping_add(1);
    }

    /// Replace the whole cache with a listing the host returned, so a
    /// reconnecting session drops tasks the host no longer holds.
    ///
    /// A listing is a snapshot taken before it was sent, so an ending that
    /// arrived while the read was in flight is newer than what the listing
    /// carries for that task. Keeping the held ending rather than the
    /// listing's older state means a task never falls back from ended to
    /// running; a task the listing does not hold at all is dropped, because
    /// the host no longer keeps it.
    pub(crate) fn replace_all(&mut self, tasks: Vec<BackgroundTaskSnapshot>) {
        let mut listed: BTreeMap<BackgroundTaskId, BackgroundTaskSnapshot> =
            tasks.into_iter().map(|task| (task.id, task)).collect();
        for (id, held) in &self.snapshots {
            let listed_running = listed
                .get(id)
                .is_some_and(|listed| !listed.status.is_terminal());
            if held.status.is_terminal() && listed_running {
                listed.insert(*id, held.clone());
            }
        }
        self.snapshots = listed;
        self.listed = true;
        self.revision = self.revision.wrapping_add(1);
    }

    /// Whether a listing has been received, so an absent task means the host
    /// no longer holds it rather than that no frame has arrived yet.
    pub(crate) fn listed(&self) -> bool {
        self.listed
    }

    /// The newest snapshot of one task.
    pub(crate) fn get(&self, id: BackgroundTaskId) -> Option<&BackgroundTaskSnapshot> {
        self.snapshots.get(&id)
    }

    /// Every known task, ordered by number.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &BackgroundTaskSnapshot> {
        self.snapshots.values()
    }

    /// How many tasks are still running.
    pub(crate) fn running_count(&self) -> usize {
        self.snapshots
            .values()
            .filter(|task| !task.status.is_terminal())
            .count()
    }

    /// The change count, for the row cache identity.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
}

/// The static composition of a session's named capabilities: the
/// server-reported caches plus the client-local session state each capability
/// keeps (such as the disabled-skill set). The connection, run, and transcript
/// live beside it, not inside it.
#[derive(Debug, Default)]
pub(crate) struct SessionCapabilities {
    /// The permission capability: mode, rules, directories, and verdicts.
    pub(crate) permission: PermissionCapability,
    /// The status capability: context cache, snapshot, poll stamp, and the
    /// pending-command flag.
    pub(crate) status: StatusCapability,
    /// The hooks capability: the registered-hook cache.
    pub(crate) hooks: HooksCapability,
    /// The tools capability: the discovered-tool cache.
    pub(crate) tools: ToolsCapability,
    /// The skills capability: the discovered-skill cache and the disable set.
    pub(crate) skills: SkillsCapability,
    /// The worktree capability: the linked-worktree listing cache.
    pub(crate) worktree: WorktreeCapability,
    /// The memory capability: the entry cache, the toggle states, the
    /// change-notice dedupe set, and the pending pane mutations.
    pub(crate) memory: MemoryCapability,
    /// The fleet capability: the live child status snapshots.
    pub(crate) fleet: FleetCapability,
    /// The agent-types capability: the delegatable sub-agent types.
    pub(crate) agent_types: AgentTypesCapability,
    /// The model capability: the host-reported catalog with the applied
    /// model, and the commit awaiting its reply.
    pub(crate) model: ModelCapability,
    /// The tasks capability: the newest snapshot of each background command.
    pub(crate) tasks: TasksCapability,
}

#[cfg(test)]
mod tests;
