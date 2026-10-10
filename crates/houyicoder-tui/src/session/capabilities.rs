//! The current session's named capabilities.
//!
//! Each capability owns the state its pane and status paths read, which
//! travels together as one value rather than a scatter of flat App fields.
//! This is the static capability composition from the application model: it
//! holds the server-reported caches plus the client-local session state each
//! capability keeps (such as the disabled-skill set), never a connection
//! handle or a session reader port.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use houyicoder_protocol::acp_wire::PermissionOptionKind;
use houyicoder_protocol::frontend::context::ContextBreakdown;
use houyicoder_protocol::frontend::hooks::HookEntry;
use houyicoder_protocol::frontend::permission::{
    PermissionDecisionEntry, PermissionMode, PermissionRule,
};
use houyicoder_protocol::frontend::skills::SkillEntry;
use houyicoder_protocol::frontend::status::StatusSnapshot;
use houyicoder_protocol::frontend::tools::ToolEntry;

use crate::composition::WorktreeEntry;

/// The session's permission state as the server reports it: the current mode,
/// the durable rule list, the allowed-directory list, and the ask-before-git
/// checkpoint, plus the per-tool sticky verdicts, the client-side verdict
/// audit trail, and the frame-log read position that guards the trail. The
/// caches are server-refreshed: the server owns the truth, and the pane
/// renders whatever the last reply shipped.
#[derive(Debug)]
pub(crate) struct PermissionCapability {
    /// The permission mode cache. Seeded on the first idle poll so the status
    /// bar pill renders from session start, then replaced by the
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
        }
    }
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
}
