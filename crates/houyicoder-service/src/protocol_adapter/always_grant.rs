//! What a yes-don't-ask verdict on one ask durably authorizes, and the verdict
//! options the approval card may therefore offer.
//!
//! The card and the consent router read this one decision, so the scope the
//! card promises is the scope that is installed. An ask whose scope cannot be
//! derived offers no remember option: a choice the gate would ignore is worse
//! than no choice.

use std::path::Path;

use houyicoder_permission::{
    AskSource, Effect, NETWORK_EGRESS, Rule, RuleContent, Scope, egress_outputs_safe,
};
use houyicoder_protocol::acp_wire::{PermissionOption, PermissionOptionKind};

use super::consent_rule_for;

/// What an "always" verdict on one ask can durably authorize. The approval
/// card and the consent router both read this, so what the card promises is
/// what gets installed.
pub(crate) enum AlwaysGrant {
    /// Network destinations remembered for the session.
    Network(Vec<String>),
    /// A directory authorized for path-bounds checks.
    Directory(String),
    /// A durable rule.
    Rule(houyicoder_permission::Rule),
    /// Nothing durable: the call must be answered once.
    Once,
}

/// The durable authorization an "always" verdict on this ask would install.
/// None-reason asks resolve to Once: without the gate's reason the caller
/// cannot tell what the user is being asked about, so it must not promise a
/// durable grant.
///
/// The fenced flag reports whether the session's execution fence is active. A
/// command whose file output is only safe because the fence bounds it cannot
/// be remembered when the fence is absent: the gate would ask again, so the
/// card must not offer the choice.
pub(crate) fn always_grant(
    tool_name: &str,
    input: &serde_json::Value,
    reason: Option<&houyicoder_permission::AskReason>,
    fenced: bool,
) -> AlwaysGrant {
    let Some(reason) = reason else {
        return AlwaysGrant::Once;
    };
    if reason.source == AskSource::SystemSafety {
        return AlwaysGrant::Once;
    }
    if matches!(reason.validator, "path-bounds" | "directory-capability") {
        return match extract_boundary_directory(&reason.detail, input) {
            Some(dir) => AlwaysGrant::Directory(dir),
            None => AlwaysGrant::Once,
        };
    }
    if matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "bash" | "sh" | "exec" | "shell"
    ) && reason.validator == NETWORK_EGRESS
    {
        // A destination is not a command rule: the network check recognizes
        // programs, not hosts, so a remembered destination is its own entry.
        let command = houyicoder_permission::input_content(tool_name, Some(input));
        if !egress_outputs_safe(&command, fenced) {
            return AlwaysGrant::Once;
        }
        return match houyicoder_permission::egress_scope_list(&command, fenced) {
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
    match consent_rule_for(tool_name, input) {
        Some(rule) => AlwaysGrant::Rule(rule),
        None => AlwaysGrant::Once,
    }
}

/// The text the approval card shows after "don't ask again for".
fn always_label(grant: &AlwaysGrant) -> Option<String> {
    match grant {
        AlwaysGrant::Once => None,
        AlwaysGrant::Network(scopes) => match scopes.as_slice() {
            [] => None,
            [one] => Some(format!("{one} *")),
            [first, second] => Some(format!("{first}, {second} *")),
            [first, second, rest @ ..] => {
                Some(format!("{first}, {second} (+{} more) *", rest.len()))
            }
        },
        AlwaysGrant::Directory(dir) => Some(dir.clone()),
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
    fenced: bool,
) -> Vec<PermissionOption> {
    let mut options = vec![PermissionOption {
        option_id: "allow_once".into(),
        name: "Allow once".into(),
        kind: PermissionOptionKind::AllowOnce,
        meta: None,
    }];
    if let Some(label) = always_label(&always_grant(tool_name, input, reason, fenced)) {
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

/// Extract the directory named by a path-bounds or directory-capability reason,
/// falling back to the parent directory of the tool's path input.
fn extract_boundary_directory(detail: &str, input: &serde_json::Value) -> Option<String> {
    if let Some((_, after)) = detail.split_once("directory ") {
        let dir = [" with ", "; choosing", " and ", " for this session"]
            .iter()
            .fold(after, |acc, sep| acc.split(sep).next().unwrap_or(acc))
            .trim();
        if !dir.is_empty() {
            return Some(dir.to_string());
        }
    }
    let raw = input.get("path").and_then(|v| v.as_str())?;
    let path = Path::new(raw);
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(path);
    Some(dir.to_string_lossy().into_owned())
}
