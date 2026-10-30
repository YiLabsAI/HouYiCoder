//! Tool-approval prompt state and option layout rules.

use std::time::{Duration, Instant};

use houyicoder_protocol::acp_wire::{PermissionOption, PermissionOptionKind};
use houyicoder_protocol::extension::ENTITLEMENT_TOOL;
use houyicoder_protocol::frontend::permission::AskSource;
use houyicoder_protocol::frontend::run::{ApprovalScope, DelegationSource};

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
    /// produced, or a generic prompt when the ask carried no reason.
    pub reason: String,
    /// The source of the ask, when the wire carried a reason. Drives the
    /// card: a SystemSafety source hides the "remember" option because
    /// consent cannot override a protected-path check.
    pub source: Option<AskSource>,
    /// The gate validator that produced the ask, when the wire carried a
    /// reason. Drives the auto-approve countdown: a card from a validator
    /// that widens the workspace bounds, touches the network, or forces a
    /// push never auto-approves.
    pub validator: Option<String>,
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
    /// Whether the argument block asks for the whole of its content instead of
    /// the bounded head and tail. The bound leaves the question and the
    /// options their rows; expanding trades those rows back for the part the
    /// bound released, so even an expanded block still gives way while the
    /// card is short.
    pub expanded: bool,
    /// The agent-loop call_id this popup resolves.
    pub call_id: String,
    /// The verdict options the server offered (N-option). Empty when the
    /// server deferred; the card falls back to its built-in 3-option set.
    pub options: Vec<PermissionOption>,
    /// The instant a Level 1 auto-approve countdown expires, when one is
    /// running. None when the card is not counting down: a Level 2 ask, a
    /// cancelled countdown, or a delay of zero.
    pub auto_approve_at: Option<Instant>,
    /// Whether the user touched a navigation key while the countdown ran.
    /// One touch cancels the auto-approve for the life of this card; the
    /// user's Enter or Esc then decides it.
    pub auto_approve_cancelled: bool,
}

/// The number of visible options in the approval prompt.
pub const APPROVAL_OPTIONS: usize = 3;

impl Approval {
    /// The scope the server attached to its remember option, when it offered
    /// one. The server is the authority on what a yes-always installs, so the
    /// card shows that text instead of deriving one from the command.
    pub fn remember_name(&self) -> Option<&str> {
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
        !self.options.is_empty() && self.remember_name().is_none()
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

    /// Whether this card may auto-approve in Auto mode after the countdown.
    /// Level 2 asks never do: a protected path, a request to widen the
    /// workspace bounds, a network call, a forced push, or a native tool call
    /// the user did not flag as discarding all need a human press. The rest
    /// (a fenced compound command, a destructive command, a local git
    /// checkpoint, a discard-flagged native call) recover or stay inside the
    /// fence, so a countdown that lands on Yes-once is safe.
    pub fn auto_approve_eligible(&self) -> bool {
        if matches!(self.source, Some(AskSource::SystemSafety)) || self.is_entitlement() {
            return false;
        }
        match self.validator.as_deref() {
            Some(
                "path_bounds"
                | "path-bounds"
                | "directory_capability"
                | "directory-capability"
                | "network_egress"
                | "network-egress",
            ) => {
                return false;
            }
            Some("git_checkpoint") => return !self.is_forced_push(),
            Some("tool_native") => return self.discards_changes(),
            _ => {}
        }
        true
    }

    /// Whether the serialized args name a forced push, the one git checkpoint
    /// form that rewrites published history rather than local state.
    fn is_forced_push(&self) -> bool {
        let args = self.args.to_ascii_lowercase();
        args.contains("push")
            && (args.contains("--force")
                || args.contains("--no-verify")
                || args.contains(" -f ")
                || args.ends_with(" -f")
                || args.contains(" -f\n"))
    }

    /// Whether the serialized args carry an explicit discard_changes true, the
    /// flag a native tool call uses to say it throws uncommitted work away.
    fn discards_changes(&self) -> bool {
        self.args.contains("\"discard_changes\"") && self.args.contains("true")
    }

    /// Start the auto-approve countdown if the card is eligible and the delay
    /// is non-zero. A Level 2 card, a zero delay, or a cursor parked on No by a
    /// sticky reject leaves the deadline None so the card waits for the user.
    pub fn start_auto_approve(&mut self, delay_secs: u64) {
        if delay_secs > 0 && self.focused_approves() && self.auto_approve_eligible() {
            self.auto_approve_at = Some(Instant::now() + Duration::from_secs(delay_secs));
            self.auto_approve_cancelled = false;
        }
    }

    /// Cancel the countdown for the life of this card. One navigation key does
    /// it; the user's Enter or Esc then decides.
    pub fn cancel_auto_approve(&mut self) {
        self.auto_approve_at = None;
        self.auto_approve_cancelled = true;
    }

    /// The whole seconds left on the countdown, for the "(auto in Ns)" hint.
    /// None when no countdown is running.
    pub fn auto_approve_secs_left(&self, now: Instant) -> Option<u64> {
        let deadline = self.auto_approve_at?;
        let left = deadline.saturating_duration_since(now).as_secs();
        Some(left)
    }

    /// Whether the countdown has run down. The caller auto-approves Once when
    /// this returns true and clears the deadline.
    pub fn auto_approve_expired(&self, now: Instant) -> bool {
        self.auto_approve_at.is_some_and(|d| now >= d)
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

    /// The lifetime the server declared for the focused option, when it
    /// declared one. The server computed it from the ask's plan, so a client
    /// reads it instead of guessing; a card the server did not fill in has no
    /// declaration and the caller falls back to its own reading.
    pub fn focused_scope(&self) -> Option<ApprovalScope> {
        let kind = self.focused_kind();
        self.options
            .iter()
            .find(|option| option.kind == kind)
            .and_then(|option| option.scope)
    }

    /// The lifetime the remember option declares, independent of which row is
    /// focused. The card prints it as a tail on that row and must keep it
    /// readable when the row is clipped.
    pub fn remember_scope(&self) -> Option<ApprovalScope> {
        self.options
            .iter()
            .find(|option| matches!(option.kind, PermissionOptionKind::AllowAlways))
            .and_then(|option| option.scope)
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

#[cfg(test)]
mod tests;
