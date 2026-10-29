//! What a yes-don't-ask verdict on one ask durably authorizes, and the verdict
//! options the approval card may therefore offer.
//!
//! The card and the consent router read this one decision, so the scope the
//! card promises is the scope that is installed. An ask whose scope cannot be
//! derived offers no remember option: a choice the gate would ignore is worse
//! than no choice.

use houyicoder_api::sandbox::BoundaryGrant;
use houyicoder_permission::{
    Effect, NETWORK_EGRESS, Rule, RuleContent, Scope, egress_outputs_safe,
};
use houyicoder_protocol::acp_wire::{PermissionOption, PermissionOptionKind};
use houyicoder_protocol::frontend::permission::{AskReason, AskSource};
use houyicoder_protocol::frontend::run::ApprovalScope;

use super::consent_rule_for;

/// What the session fence will do to the call the card describes, reduced to
/// the two facts the option list needs. Writes confinement decides whether a
/// command whose local output is bounded can be remembered at all; an egress
/// refusal decides whether a remembered destination is a promise the kernel
/// keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CardFence {
    /// Whether the fence confines where writes land.
    pub writes_confined: bool,
    /// Whether the fence will refuse the egress this call asks for.
    pub egress_blocked: bool,
}

/// A fence that confines both filesystem axes and permits egress: the state
/// every path fence reports.
#[cfg(test)]
pub(crate) const FENCED: CardFence = CardFence {
    writes_confined: true,
    egress_blocked: false,
};

/// A fence that confines neither axis.
#[cfg(test)]
pub(crate) const UNFENCED: CardFence = CardFence {
    writes_confined: false,
    egress_blocked: false,
};

/// What an "always" verdict on one ask can durably authorize. The approval
/// card and the consent router both read this, so what the card promises is
/// what gets installed.
pub(crate) enum AlwaysGrant {
    /// Network destinations remembered for the session.
    Network(Vec<String>),
    /// A directory capability. The directories themselves are the grants the
    /// caller installs; this branch says the verdict has one to install and
    /// the card may therefore offer it.
    Directories,
    /// A durable rule.
    Rule(houyicoder_permission::Rule),
    /// Nothing durable: the call must be answered once.
    Once,
}

impl AlwaysGrant {
    /// The lifetime the card states and the install keeps. A destination or a
    /// git history rule lasts this session: the approval was about the work
    /// in progress, and the git rule already carries that scope. A directory
    /// capability outlives the session because the fence rehydrates it on the
    /// next start, and the restore drops it once the directory no longer
    /// names the same place. An ordinary command rule lands in the
    /// machine-local store, where the workspace lifetime restores it and a
    /// different checkout never inherits it.
    pub(crate) fn scope(&self) -> Option<ApprovalScope> {
        match self {
            Self::Once => None,
            Self::Network(_) => Some(ApprovalScope::Session),
            Self::Directories => Some(ApprovalScope::Workspace),
            Self::Rule(rule) => Some(match rule.scope {
                Scope::Session => ApprovalScope::Session,
                _ => ApprovalScope::Workspace,
            }),
        }
    }
}

/// The shorter of two lifetimes. The client's choice decides the lifetime the
/// install keeps, and it can only shorten what the grant plan allows; a client
/// that names a longer one than the plan supports still installs the plan's
/// lifetime.
fn shorter(choice: ApprovalScope, allowed: ApprovalScope) -> ApprovalScope {
    match (choice, allowed) {
        (ApprovalScope::Once, _) | (_, ApprovalScope::Once) => ApprovalScope::Once,
        (ApprovalScope::Session, _) | (_, ApprovalScope::Session) => ApprovalScope::Session,
        _ => ApprovalScope::Workspace,
    }
}

/// The lifetime an install keeps for this grant when the client chose choice:
/// nothing when the choice is once or the grant has no lifetime, otherwise the
/// shorter of the choice and the plan.
pub(crate) fn installed_scope(grant: &AlwaysGrant, choice: ApprovalScope) -> ApprovalScope {
    match grant.scope() {
        Some(allowed) => shorter(choice, allowed),
        None => ApprovalScope::Once,
    }
}

/// The durable authorization an "always" verdict on this ask would install.
/// None-reason asks resolve to Once: without the gate's reason the caller
/// cannot tell what the user is being asked about, so it must not promise a
/// durable grant.
///
/// A command whose local file output is only safe because the fence bounds it
/// cannot be remembered when the fence does not bound writes: the gate would
/// ask again, so the card must not offer the choice. A destination the fence
/// will refuse is refused here as well, for the same reason one step later:
/// the remembered entry would change nothing at execution time, so the card
/// would collect an answer the kernel overrules.
///
/// A directory-bounds ask takes its directories from the grants the caller is
/// about to install, not from the reason text: the card writes that text, and
/// reading a path back out of prose installs whatever the wording happens to
/// contain. Every grant is listed, because the install covers all of them.
pub(crate) fn always_grant(
    tool_name: &str,
    input: &serde_json::Value,
    reason: Option<&AskReason>,
    fence: CardFence,
    grants: &[BoundaryGrant],
) -> AlwaysGrant {
    let Some(reason) = reason else {
        return AlwaysGrant::Once;
    };
    if reason.source == AskSource::SystemSafety {
        return AlwaysGrant::Once;
    }
    if matches!(
        reason.validator.as_str(),
        "path-bounds" | "directory-capability"
    ) {
        return if grants.is_empty() {
            AlwaysGrant::Once
        } else {
            AlwaysGrant::Directories
        };
    }
    if matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "bash" | "sh" | "exec" | "shell"
    ) && reason.validator == NETWORK_EGRESS
    {
        // A destination is not a command rule: the network check recognizes
        // programs, not hosts, so a remembered destination is its own entry.
        // A fence that refuses the egress keeps the ask single-shot.
        if fence.egress_blocked {
            return AlwaysGrant::Once;
        }
        let command = houyicoder_permission::input_content(tool_name, Some(input));
        if !egress_outputs_safe(&command, fence.writes_confined) {
            return AlwaysGrant::Once;
        }
        return match houyicoder_permission::egress_scope_list(&command, fence.writes_confined) {
            Some(scopes) if !scopes.is_empty() => AlwaysGrant::Network(scopes),
            _ => AlwaysGrant::Once,
        };
    }
    // The four history-moving git subcommands shadow their builtin ask rule
    // for this session only: the approval is about one commit, not about every
    // future one.
    let command = houyicoder_permission::input_content(tool_name, Some(input));
    if let Some(git_cmd) = houyicoder_permission::classify_git_op(tool_name, &command) {
        if matches!(git_cmd, "commit" | "rebase" | "reset" | "tag")
            && let Ok(rule) = Rule::with_content(
                "bash",
                RuleContent::Prefix(format!("git {git_cmd}")),
                Effect::Allow,
            )
        {
            return AlwaysGrant::Rule(rule.with_scope(Scope::Session));
        }
        return AlwaysGrant::Once;
    }
    // A remembered command is a preference about this machine's workspace,
    // not a policy the repository should carry: it lands in the machine-local
    // store, where the workspace lifetime can restore it and where a different
    // checkout never inherits it.
    match consent_rule_for(tool_name, input) {
        Some(rule) => AlwaysGrant::Rule(rule.with_scope(Scope::Local)),
        None => AlwaysGrant::Once,
    }
}

/// The directories a remember option names, taken from every grant the
/// verdict would install. One grant is listed in full; a longer list keeps
/// its first two and counts the rest, so the row stays one line while the
/// count still says the install reaches further.
fn directory_label(grants: &[BoundaryGrant]) -> Option<String> {
    let listed: Vec<String> = grants
        .iter()
        .map(|grant| grant.directory.display().to_string())
        .collect();
    match listed.split_first() {
        None => None,
        Some((first, [])) => Some(first.clone()),
        Some((first, [second])) => Some(format!("{first}, {second}")),
        Some((first, [second, rest @ ..])) => {
            Some(format!("{first}, {second} (+{} more)", rest.len()))
        }
    }
}

/// The text the approval card shows after "don't ask again for": the effect
/// the choice authorizes and the lifetime it lasts. Every directory grant is
/// named, because the install covers all of them. Naming the lifetime is what
/// lets the user tell a session-long preference from a workspace-long one
/// without knowing which store each lands in.
fn always_label(grant: &AlwaysGrant, grants: &[BoundaryGrant]) -> Option<String> {
    let effect = match grant {
        AlwaysGrant::Once => return None,
        AlwaysGrant::Network(scopes) => match scopes.as_slice() {
            [] => return None,
            [one] => format!("{one} *"),
            [first, second] => format!("{first}, {second} *"),
            [first, second, rest @ ..] => {
                format!("{first}, {second} (+{} more) *", rest.len())
            }
        },
        AlwaysGrant::Directories => directory_label(grants)?,
        AlwaysGrant::Rule(rule) => match &rule.content {
            Some(RuleContent::Prefix(prefix)) => format!("{prefix} *"),
            Some(RuleContent::Exact(value)) => value.clone(),
            Some(RuleContent::Glob(pattern)) => pattern.clone(),
            None => rule.action.clone(),
        },
    };
    let lifetime = match grant.scope()? {
        ApprovalScope::Session => "this session",
        ApprovalScope::Workspace => "this workspace",
        ApprovalScope::Once => return None,
    };
    Some(format!("{effect} ({lifetime})"))
}

/// The verdict options the card offers for this ask. The remember option is
/// present only when the server can install the scope it names, so the card
/// never offers a choice the gate would ignore.
pub(crate) fn approval_options(
    tool_name: &str,
    input: &serde_json::Value,
    reason: Option<&AskReason>,
    fence: CardFence,
    grants: &[BoundaryGrant],
) -> Vec<PermissionOption> {
    let mut options = vec![PermissionOption {
        option_id: "allow_once".into(),
        name: "Allow once".into(),
        kind: PermissionOptionKind::AllowOnce,
        scope: None,
        meta: None,
    }];
    let grant = always_grant(tool_name, input, reason, fence, grants);
    if let Some(label) = always_label(&grant, grants) {
        options.push(PermissionOption {
            option_id: "allow_always".into(),
            name: label,
            kind: PermissionOptionKind::AllowAlways,
            scope: grant.scope(),
            meta: None,
        });
    }
    options.push(PermissionOption {
        option_id: "reject_once".into(),
        name: "Reject once".into(),
        kind: PermissionOptionKind::RejectOnce,
        scope: None,
        meta: None,
    });
    options
}
