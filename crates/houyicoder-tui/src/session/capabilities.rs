//! The current session's named capabilities.
//!
//! Each capability owns the state its pane and status paths read, which
//! travels together as one value rather than a scatter of flat App fields.
//! This is the static capability composition from the application model: it
//! holds the server-reported caches plus the client-local session state each
//! capability keeps (such as the disabled-skill set), never a connection
//! handle or a session reader port.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use houyicoder_protocol::acp_wire::PermissionOptionKind;
use houyicoder_protocol::frontend::context::ContextBreakdown;
use houyicoder_protocol::frontend::hooks::HookEntry;
use houyicoder_protocol::frontend::permission::{
    PermissionDecisionEntry, PermissionMode, PermissionRule,
};
use houyicoder_protocol::frontend::skills::SkillEntry;
use houyicoder_protocol::frontend::status::StatusSnapshot;
use houyicoder_protocol::frontend::tools::ToolEntry;

/// The session's permission state as the wire reports it: the current mode,
/// the durable rule list, the allowed-directory list, and the ask-before-git
/// checkpoint, plus the per-tool sticky verdicts and the client-side verdict
/// audit trail. All of it is wire-refreshed cache; the server owns the truth,
/// the pane renders whatever the last reply shipped.
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
    /// True when a /status command is awaiting a wire reply. The periodic poll
    /// updates the cache silently; a command-initiated poll also renders the
    /// full status block as a transcript line on reply. Cleared on reply.
    pub(crate) pending_status_command: bool,
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
}
