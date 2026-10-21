//! Tool-approval prompt state and option layout rules.

use houyicoder_protocol::acp_wire::{PermissionOption, PermissionOptionKind};
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use houyicoder_protocol::frontend::permission::AskSource;
use houyicoder_protocol::frontend::run::DelegationSource;

/// Tool-approval prompt state. Three options: approve, reject, or
/// always-allow (approve + add a permission rule so the same tool no
/// longer prompts). call_id links the prompt back to the
/// ApprovalRequest from the agent loop so resume() gets the right decision.
#[derive(Debug, Clone, Default)]
pub struct Approval {
    /// Tool name awaiting approval.
    pub tool: String,
    /// Serialized arguments (placeholder).
    pub args: String,
    /// Why the tool wants to run: the structured AskReason detail the gate
    /// produced, or a generic prompt when the composition root could not
    /// reconstruct one.
    pub reason: String,
    /// The source of the ask, when the wire carried a reason. Drives the
    /// card: a SystemSafety source hides the "remember" option because
    /// consent cannot override a protected-path check.
    pub source: Option<AskSource>,
    /// The child delegation this ask was routed up from, when the ask
    /// originates from a spawned child rather than the parent's own run.
    /// None for a parent tool call. The card labels the child type so the
    /// user can tell a child's ask from the parent's when delegations are
    /// in flight.
    pub delegation: Option<DelegationSource>,
    /// An optional note from the containment layer the card renders as a
    /// fourth line when present.
    pub containment_note: Option<String>,
    /// The index of the currently highlighted option (0-based).
    pub selected: usize,
    /// The agent-loop call_id this popup resolves.
    pub call_id: String,
    /// The verdict options the server offered (N-option). Empty when the
    /// server deferred; the card falls back to its built-in 3-option set.
    pub options: Vec<PermissionOption>,
}

/// The number of visible options in the approval prompt.
pub const APPROVAL_OPTIONS: usize = 3;

impl Approval {
    /// The scope the server attached to its remember option, when it offered
    /// one. The server is the authority on what a yes-always installs, so the
    /// card shows that text instead of deriving one from the command.
    pub fn remember_scope(&self) -> Option<&str> {
        self.options
            .iter()
            .find(|option| matches!(option.kind, PermissionOptionKind::AllowAlways))
            .map(|option| option.name.as_str())
            .filter(|name| !name.is_empty())
    }

    /// Whether the "Yes, and don't ask again" option is hidden. A protected-
    /// path check (source SystemSafety) is authoritative: a stored consent
    /// rule cannot override it, so persisting a yes-always would be a no-op
    /// at best and misleading at worst. A server that sent an explicit option
    /// set is equally authoritative: when it offered no remember option, it
    /// can install no scope for this call.
    pub fn remember_hidden(&self) -> bool {
        if matches!(self.source, Some(AskSource::SystemSafety)) {
            return true;
        }
        !self.options.is_empty() && self.remember_scope().is_none()
    }

    /// Whether this is an entitlement ask (a deny-log discovery offering
    /// to authorize blocked mach services for a skill).
    pub fn is_entitlement(&self) -> bool {
        self.tool == ENTITLEMENT_TOOL
    }

    /// Whether the card renders a two-option form: a protected-path ask
    /// hides persistence, while an entitlement ask offers a persistent
    /// allow or a decline.
    pub fn two_option_card(&self) -> bool {
        self.remember_hidden() || self.is_entitlement()
    }

    /// The count of visible options: two (Yes / No) on a two-option card,
    /// otherwise the built-in three.
    pub fn visible_option_count(&self) -> usize {
        if self.two_option_card() {
            2
        } else {
            APPROVAL_OPTIONS
        }
    }

    /// Label for the currently focused button.
    pub fn focus_label(&self) -> &'static str {
        match self.selected {
            0 => "yes",
            1 => "no",
            _ => "yes-dont-ask-again",
        }
    }

    /// True when the focused option approves the tool call.
    pub fn focused_approves(&self) -> bool {
        self.selected != 1
    }

    /// True when the focused option is Yes-don't-ask-again.
    pub fn focused_persists(&self) -> bool {
        self.selected == 2
    }

    /// The identity of the focused option for the built-in 3-option set.
    pub fn focused_kind(&self) -> PermissionOptionKind {
        match self.selected {
            0 => PermissionOptionKind::AllowOnce,
            1 => PermissionOptionKind::RejectOnce,
            _ => PermissionOptionKind::AllowAlways,
        }
    }

    /// The built-in 3-option index whose kind matches.
    pub fn index_for_kind(kind: PermissionOptionKind) -> usize {
        match kind {
            PermissionOptionKind::AllowOnce => 0,
            PermissionOptionKind::AllowAlways => 2,
            PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => 1,
        }
    }
}
