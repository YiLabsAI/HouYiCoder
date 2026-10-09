//! The current session's named capabilities as the wire reports them.
//!
//! Each capability owns the caches its pane and status paths read, which
//! travel together as one value rather than a scatter of flat App fields. This
//! is the static capability composition from
//! the application model: it holds data the wire declared, never a
//! connection handle or a session reader port.

use std::collections::HashMap;

use houyicoder_protocol::acp_wire::PermissionOptionKind;
use houyicoder_protocol::frontend::permission::{
    PermissionDecisionEntry, PermissionMode, PermissionRule,
};

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

/// The static composition of a session's named capabilities. It holds exactly
/// the state the wire declared for this session and nothing else; the
/// connection, run, and transcript live beside it, not inside it.
#[derive(Debug, Default)]
pub(crate) struct SessionCapabilities {
    /// The permission capability: mode, rules, directories, and verdicts.
    pub(crate) permission: PermissionCapability,
}
