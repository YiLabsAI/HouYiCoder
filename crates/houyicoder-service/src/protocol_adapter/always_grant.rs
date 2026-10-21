//! What a yes-don't-ask verdict on one ask durably authorizes, and the verdict
//! options the approval card may therefore offer.
//!
//! The card and the consent router read this one decision, so the scope the
//! card promises is the scope that is installed. An ask whose scope cannot be
//! derived offers no remember option: a choice the gate would ignore is worse
//! than no choice.

use houyicoder_permission::{AskSource, Effect, NETWORK_EGRESS, Rule, RuleContent, Scope};
use houyicoder_protocol::acp_wire::{PermissionOption, PermissionOptionKind};

use super::consent_rule_for;

/// What an "always" verdict on one ask can durably authorize. The approval
/// card and the consent router both read this, so what the card promises is
/// what gets installed.
pub(crate) enum AlwaysGrant {
    /// One network destination, remembered for the session.
    Network(String),
    /// A durable rule.
    Rule(houyicoder_permission::Rule),
    /// Nothing durable: the call must be answered once.
    Once,
}

/// The durable authorization an "always" verdict on this ask would install.
/// None-reason asks resolve to Once: without the gate's reason the caller
/// cannot tell what the user is being asked about, so it must not promise a
/// durable grant.
pub(crate) fn always_grant(
    tool_name: &str,
    input: &serde_json::Value,
    reason: Option<&houyicoder_permission::AskReason>,
) -> AlwaysGrant {
    let Some(reason) = reason else {
        return AlwaysGrant::Once;
    };
    if reason.source == AskSource::SystemSafety {
        return AlwaysGrant::Once;
    }
    if matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "bash" | "sh" | "exec" | "shell"
    ) && reason.validator == NETWORK_EGRESS
    {
        // A destination is not a command rule: the network check recognizes
        // programs, not hosts, so a remembered destination is its own entry.
        let command = houyicoder_permission::input_content(tool_name, Some(input));
        return match houyicoder_permission::egress_destination(&command) {
            Some(destination) => AlwaysGrant::Network(destination),
            None => AlwaysGrant::Once,
        };
    }
    // The four history-moving git subcommands shadow their builtin ask rule
    // for this session only: the approval is about one commit, not about every
    // future one.
    let command = houyicoder_permission::input_content(tool_name, Some(input));
    if let Some(git_cmd) = houyicoder_permission::classify_git_op(tool_name, &command)
        && matches!(git_cmd, "commit" | "rebase" | "reset" | "tag")
        && let Ok(rule) = Rule::with_content(
            "bash",
            RuleContent::Prefix(format!("git {git_cmd}")),
            Effect::Allow,
        )
    {
        return AlwaysGrant::Rule(rule.with_scope(Scope::Session));
    }
    match consent_rule_for(tool_name, input) {
        Some(rule) => AlwaysGrant::Rule(rule),
        None => AlwaysGrant::Once,
    }
}

/// The text the approval card shows after "don't ask again for".
fn always_label(grant: &AlwaysGrant) -> Option<String> {
    match grant {
        AlwaysGrant::Once => None,
        AlwaysGrant::Network(destination) => Some(format!("{destination} *")),
        AlwaysGrant::Rule(rule) => Some(match &rule.content {
            Some(RuleContent::Prefix(prefix)) => format!("{prefix} *"),
            Some(RuleContent::Exact(value)) => value.clone(),
            Some(RuleContent::Glob(pattern)) => pattern.clone(),
            None => rule.action.clone(),
        }),
    }
}

/// The verdict options the card offers for this ask. The remember option is
/// present only when the server can install the scope it names, so the card
/// never offers a choice the gate would ignore.
pub(crate) fn approval_options(
    tool_name: &str,
    input: &serde_json::Value,
    reason: Option<&houyicoder_permission::AskReason>,
) -> Vec<PermissionOption> {
    let mut options = vec![PermissionOption {
        option_id: "allow_once".into(),
        name: "Allow once".into(),
        kind: PermissionOptionKind::AllowOnce,
        meta: None,
    }];
    if let Some(label) = always_label(&always_grant(tool_name, input, reason)) {
        options.push(PermissionOption {
            option_id: "allow_always".into(),
            name: label,
            kind: PermissionOptionKind::AllowAlways,
            meta: None,
        });
    }
    options.push(PermissionOption {
        option_id: "reject_once".into(),
        name: "Reject once".into(),
        kind: PermissionOptionKind::RejectOnce,
        meta: None,
    });
    options
}
