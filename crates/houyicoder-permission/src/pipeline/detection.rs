//! The detection stage: deterministic heuristics that flag risky calls. These
//! are the bypass-immune safety checks that fire before the rule arms and the
//! mode default. Each validator wraps an existing predicate so the behavior is
//! preserved exactly; the validators are thin adapters over the shared
//! context, not a re-derivation of the rules.

use houyicoder_api::sandbox::{
    BoundaryAccess, BoundaryGrant, Containment, FenceAxis, boundary_grants_for,
};
use houyicoder_api::tool::is_shell_tool;
use std::sync::Arc;

use super::attribution::{compound_unverified_step, describe_destructive, describe_egress_reason};
use crate::compound::{compound_safe, is_effect_safe, is_readonly_compound, removes_only_temp};
use crate::decision::{AskReason, AskSource, Decision};
use crate::egress::{egress_outputs_safe, egress_scope_list};
use crate::git_safety::collect_git_ask_ops;
use crate::mode::{PermissionMode, ToolRequest};
use crate::pipeline::{GateCtx, Immunity, Stage, Validator, consent_allows};
use crate::rule::{Effect, RuleContent, evaluate};
use crate::shell::{ShellCommand, ShellScan, canonical_scan};

mod gh;
mod signal;

use gh::gh_reads_only;
use signal::should_ask_signal;

/// Whether the session's fence confines writes. Where the kernel decides
/// where a write lands, the gate only has to reason about constructs that
/// change what the kernel is asked to open. The axis is named rather than
/// inferred from coverage: a fence that confines reads or the network does
/// not thereby confine writes, and every relaxation below rests on writes.
pub(crate) fn writes_confined(containment: &Option<Arc<dyn Containment>>) -> bool {
    containment
        .as_ref()
        .is_some_and(|c| c.capability().confines(FenceAxis::Writes))
}

/// Programs whose run destroys files or escalates privileges. The scan matches
/// them in command position: a verb inside a quoted search pattern is data the
/// shell passes to another program, not a call.
const DESTRUCTIVE_PROGRAMS: &[&str] = &["dd", "rm", "rmdir", "su", "sudo", "truncate", "unlink"];

/// Programs that leave the machine.
const EGRESS_TOOLS: &[&str] = &[
    "curl", "ftp", "httpie", "nc", "netcat", "rsync", "scp", "sftp", "ssh", "telnet", "wget",
];

/// Destructive verbs (rm, sudo, dd, …) escalate to Ask even when the mode
/// default would Allow. Consent-overridable for an exact pre-approved call.
/// Un-attestable constructs (redirect, substitution) are escalated by the
/// compound validator, not here: destructive means a destructive program.
fn should_ask_destructive(tool_name: &str, content: &str) -> bool {
    if !is_shell_tool(tool_name) {
        return false;
    }
    if content.is_empty() {
        return false;
    }
    let scan = canonical_scan(content);
    // Code the scan cannot read as a program that only reads can do whatever
    // a program can, including delete files.
    scan.opaque_code() || destructive_command(&scan).is_some()
}

/// Whether the only effect the scan can read is a removal of scratch files,
/// under a fence that bounds where a write lands. The exemption rests on the
/// scan having read every command: code it cannot read rides along with a
/// removal without being covered by it, so such a command is not exempt.
fn scan_confines_removal_to_temp(
    content: &str,
    containment: &Option<Arc<dyn Containment>>,
) -> bool {
    !canonical_scan(content).opaque_code()
        && removes_only_temp(content, writes_confined(containment))
}

/// The first destructive invocation the scan found. An escalated invocation
/// counts whatever it runs: the launcher's privilege is the risk.
fn destructive_command(scan: &ShellScan) -> Option<&ShellCommand> {
    scan.commands()
        .iter()
        .find(|command| command.is_one_of(DESTRUCTIVE_PROGRAMS) || command.escalated().is_some())
}

/// Whether an invocation reaches outside the machine.
fn reaches_network(command: &ShellCommand) -> bool {
    if command.is_one_of(EGRESS_TOOLS) {
        return true;
    }
    // The GitHub client reaches the network unless the call only reads. The
    // read set is the inspection verbs plus a bare api request, which posts
    // only when a method or a field says so.
    if command.runs("gh") {
        return !gh_reads_only(&word_refs(command.args()));
    }
    if let Some((sub, args)) = command.git_subcommand() {
        return match sub.to_ascii_lowercase().as_str() {
            // A fetch or a remote listing against this repository's own
            // configured remotes only moves local refs: nothing of the user's
            // leaves the machine and no working file changes. It is the same
            // traffic as a package fetch, so it asks only when it names a
            // source outside the repository's configuration.
            "fetch" | "ls-remote" => git_egress_names_external_source(args),
            // These send local work out, or bring a tree in and check it out.
            "push" | "clone" | "pull" => true,
            _ => false,
        };
    }
    // Package publish: npm publish, cargo publish, pip upload.
    if command.runs("npm") || command.runs("cargo") {
        return word_is(command, 1, "publish");
    }
    if command.runs("pip") {
        return word_is(command, 1, "upload");
    }
    false
}

/// Whether the word at an index is the named word, compared without case.
fn word_is(command: &ShellCommand, index: usize, name: &str) -> bool {
    command
        .word(index)
        .is_some_and(|word| word.eq_ignore_ascii_case(name))
}

/// The words of an invocation as references, for a classifier that takes a
/// plain word slice.
fn word_refs(words: &[String]) -> Vec<&str> {
    words.iter().map(String::as_str).collect()
}

/// Detect network-egress commands in bash: curl, wget, git push, package
/// publish, scp, rsync, ssh. These carry external side effects (data leaves
/// the sandbox). The gate always asks for these — never a silent allow in
/// Auto, never a deny citing the fence. The fence blocks at execution time
/// when it does not permit egress, and post_transform attaches that verdict
/// as a containment_note so the user knows approval may not help.
pub(crate) fn should_ask_egress(tool_name: &str, content: &str) -> bool {
    if !is_shell_tool(tool_name) {
        return false;
    }
    if content.is_empty() {
        return false;
    }
    let scan = canonical_scan(content);
    scan.commands().iter().any(reaches_network)
}

/// The git subcommands that reach a remote, as the words the card and the
/// consent router name.
const GIT_NETWORK_OPS: &[&str] = &["push", "pull", "clone", "fetch", "ls-remote"];

/// The git network subcommand a command names, when the first one it runs is
/// such a call.
///
/// The consent router reads this to build the rule a remembered answer
/// installs, and the network check reads it to honor that rule, so both name
/// the same subcommand.
pub fn egress_remote_op(content: &str) -> Option<&'static str> {
    let scan = canonical_scan(content);
    for command in scan.commands() {
        let Some((sub, _)) = command.git_subcommand() else {
            continue;
        };
        if let Some(op) = GIT_NETWORK_OPS
            .iter()
            .find(|op| op.eq_ignore_ascii_case(sub))
        {
            return Some(op);
        }
    }
    None
}

/// Whether a command names a network destination the user never configured.
///
/// A bare fetch or remote listing reads the remotes git already has configured;
/// an explicit URL, path, or transport override names something else, so the
/// destination is unknown and the call asks. A remembered answer reads this too:
/// a session answer about configured remotes must not cover a later call that
/// names an arbitrary host.
pub fn egress_names_external_source(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    // Rebinding the transport picks the program that speaks to the remote, so
    // the destination is no longer the one the repository configured.
    if lower.contains("git_ssh_command=") || lower.contains("git_ssh=") {
        return true;
    }
    canonical_scan(content).commands().iter().any(|command| {
        command
            .git_subcommand()
            .is_some_and(|(_, args)| git_egress_names_external_source(args))
    })
}

fn git_egress_names_external_source(args: &[String]) -> bool {
    args.iter().any(|arg| {
        arg.contains("://")
            || arg.starts_with("git@")
            || arg.starts_with("--upload-pack")
            // The ssh transport's own spelling: a host and a path joined by a
            // colon, with an optional user. A refspec also carries a colon, so
            // this reads one as a destination and asks rather than guessing.
            || (arg.contains(':') && !arg.starts_with('-'))
            || arg.starts_with('/')
            || arg.starts_with('.')
            || arg.starts_with('~')
    })
}

/// Strip a matching pair of surrounding quotes from a token. Used so a quoted
/// subcommand like git "push" (the sub token literally includes the quote
/// characters) matches the bare push word. Shared with the git-discard
/// classifier, which calls it for the same reason.
pub(crate) fn strip_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if s.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[0] == b[s.len() - 1] {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// rm / sudo / un-attestable redirects and substitution. Consent-overridable.
pub struct DestructiveCommandValidator {
    containment: Option<Arc<dyn Containment>>,
}

impl DestructiveCommandValidator {
    pub fn new(containment: Option<Arc<dyn Containment>>) -> Self {
        Self { containment }
    }
}

impl Validator for DestructiveCommandValidator {
    fn name(&self) -> &'static str {
        "destructive_command"
    }
    fn stage(&self) -> Stage {
        Stage::Detection
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeImmune
    }
    fn consent_overridable(&self) -> bool {
        true
    }
    fn check(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        if !should_ask_destructive(req.tool_name, ctx.content) {
            return None;
        }
        if ctx.mode == PermissionMode::Auto
            && scan_confines_removal_to_temp(ctx.content, &self.containment)
        {
            return None;
        }
        if consent_allows(req, ctx) {
            return None;
        }
        Some(Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: self.name(),
            detail: describe_destructive(ctx.content),
            containment_note: None,
        }))
    }
}

/// Sending a signal asks for a human confirm.
///
/// The kernel permits the signal so an approved one works; this is where the
/// approval is asked for. Without it an Auto-mode kill would reach any process
/// on the host with nothing having decided it should.
pub struct SignalCommandValidator;

impl Validator for SignalCommandValidator {
    fn name(&self) -> &'static str {
        "signal_command"
    }
    fn stage(&self) -> Stage {
        Stage::Detection
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeImmune
    }
    fn consent_overridable(&self) -> bool {
        true
    }
    fn check(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        if !should_ask_signal(req.tool_name, ctx.content) {
            return None;
        }
        if consent_allows(req, ctx) {
            return None;
        }
        Some(Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: self.name(),
            detail: "a command that signals another process needs confirmation".into(),
            containment_note: None,
        }))
    }
}

/// The git checkpoint: commit / rebase / reset / tag ask for a human confirm
/// even though they are recoverable via reflog. Suppressed by an allow-rule,
/// by the session consent, or by the toggle. Stays as one validator; the
/// checkpoint arms live inline here, not in builtin rules, and only the
/// discard detection is separate.
pub struct GitCheckpointValidator;

impl Validator for GitCheckpointValidator {
    fn name(&self) -> &'static str {
        "git_checkpoint"
    }
    fn stage(&self) -> Stage {
        Stage::Detection
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeImmune
    }
    fn consent_overridable(&self) -> bool {
        true
    }
    fn check(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        let git_ops = collect_git_ask_ops(req.tool_name, ctx.content);
        if git_ops.is_empty() {
            return None;
        }
        let single_op = git_ops.len() == 1;
        for git_word in git_ops {
            let is_checkpoint = matches!(git_word, "commit" | "rebase" | "reset" | "tag");
            if is_checkpoint && !ctx.git_checkpoint_enabled {
                continue;
            }
            if git_word != "stash" {
                let rule_allows = (single_op && ctx.effect == Some(Effect::Allow))
                    || (is_checkpoint
                        && evaluate(ctx.rules, req.tool_name, &format!("git {git_word}"))
                            == Some(Effect::Allow));
                if rule_allows || (single_op && consent_allows(req, ctx)) {
                    continue;
                }
            }
            let detail = match git_word {
                "stash" => {
                    "git stash modifies the shared stash stack across all worktrees and needs confirmation"
                        .into()
                }
                "commit" | "rebase" | "reset" | "tag" => {
                    format!("git {git_word} modifies repository commit history and needs confirmation")
                }
                "checkout" | "restore" | "clean" | "switch" => {
                    format!("git {git_word} discards uncommitted working-tree changes and needs confirmation")
                }
                other => format!("git {other} rewrites repository state and needs confirmation"),
            };
            return Some(Decision::Ask(AskReason {
                source: AskSource::Detection,
                validator: self.name(),
                detail,
                containment_note: None,
            }));
        }
        None
    }
}

/// Network egress: curl, wget, git push, package publish, scp, rsync, ssh.
/// Asks unless the session remembered this command's destination. The fence is
/// the single authority over containment: when it does not permit egress the
/// kernel refuses at execution time, and post_transform attaches the fence's
/// verdict (via would_block) as a containment_note so the user sees that
/// approval will not help. The gate never denies on containment grounds — that
/// would make it a second authority beside the fence.
pub struct NetworkEgressValidator {
    containment: Option<Arc<dyn Containment>>,
}

impl NetworkEgressValidator {
    pub fn new(containment: Option<Arc<dyn Containment>>) -> Self {
        Self { containment }
    }
}

/// The stable name of the network-egress validator. A metrics bucket key and
/// the recognizer post_transform uses to query the fence at the network layer
/// (an egress call runs via bash so its side-effect classification is Exec,
/// but the fence blocks egress, not exec).
pub const NETWORK_EGRESS: &str = "network_egress";

impl Validator for NetworkEgressValidator {
    fn name(&self) -> &'static str {
        NETWORK_EGRESS
    }
    fn stage(&self) -> Stage {
        Stage::Detection
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeImmune
    }
    fn consent_overridable(&self) -> bool {
        true
    }
    fn check(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        if !should_ask_egress(req.tool_name, ctx.content) {
            return None;
        }
        let outputs_safe = egress_outputs_safe(ctx.content, writes_confined(&self.containment));
        if outputs_safe
            && let Some(scopes) = egress_scope_list(ctx.content, writes_confined(&self.containment))
            && !scopes.is_empty()
            && scopes
                .iter()
                .all(|dest| ctx.egress_allow.iter().any(|allowed| allowed == dest))
        {
            return None;
        }
        if remembered_network_op(req, ctx) {
            return None;
        }
        if consent_allows(req, ctx) {
            return None;
        }
        Some(Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: self.name(),
            detail: describe_egress_reason(ctx.content, outputs_safe),
            containment_note: None,
        }))
    }
}

/// Whether the user remembered this network subcommand for the session.
///
/// Only a rule that names the subcommand counts. A tool-level allow says the
/// shell may run, not that it may reach the network, so reading it here would
/// turn one broad allow into standing egress consent.
fn remembered_network_op(req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> bool {
    // An explicit destination is never covered by a remembered remote: the
    // answer was about the remotes git already had.
    if egress_names_external_source(ctx.content) {
        return false;
    }
    // Only a lone git call is covered. A remembered answer about a remote says
    // nothing about the other commands a compound runs, and waiving the whole
    // compound would let one remembered fetch carry an arbitrary call with it.
    let scan = canonical_scan(ctx.content);
    let [only] = scan.commands() else {
        return false;
    };
    let Some((sub, _)) = only.git_subcommand() else {
        return false;
    };
    let Some(op) = GIT_NETWORK_OPS
        .iter()
        .find(|op| op.eq_ignore_ascii_case(sub))
    else {
        return false;
    };
    let prefix = format!("git {op}");
    ctx.rules.iter().any(|rule| {
        rule.effect == Effect::Allow
            && rule.action.eq_ignore_ascii_case(req.tool_name)
            && matches!(&rule.content, Some(RuleContent::Prefix(p)) if *p == prefix)
    })
}

/// A compound shell command that is not statically attestable.
pub struct CompoundCommandValidator {
    containment: Option<Arc<dyn Containment>>,
}

impl CompoundCommandValidator {
    pub fn new(containment: Option<Arc<dyn Containment>>) -> Self {
        Self { containment }
    }
}

impl Validator for CompoundCommandValidator {
    fn name(&self) -> &'static str {
        "compound_command"
    }
    fn stage(&self) -> Stage {
        Stage::Detection
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeImmune
    }
    fn consent_overridable(&self) -> bool {
        true
    }
    fn check(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        if !is_shell_tool(req.tool_name) {
            return None;
        }
        let refs: Vec<&str> = ctx.segments.iter().map(|s| s.as_str()).collect();
        if refs.is_empty() {
            return None;
        }
        if compound_safe(&refs)
            || is_readonly_compound(&refs)
            || (ctx.mode == PermissionMode::Auto
                && is_effect_safe(ctx.content, writes_confined(&self.containment)))
        {
            return None;
        }
        if consent_allows(req, ctx) {
            return None;
        }
        let detail = format!(
            "compound command contains an unverified step ({})",
            compound_unverified_step(&refs)
        );
        Some(Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: self.name(),
            detail,
            containment_note: None,
        }))
    }
}

/// Asks before a write or search tool touches a path the fence places
/// outside the workspace and authorized dirs, so approval widens the fence
/// instead of authorizing a call the fence still refuses. Paths resolve
/// through the shared normalizer; an unresolvable path asks (fail-ask), a
/// missing fence degrades to no check. Deny rules fire earlier in the
/// ladder, so this never overrides a Deny.
pub struct PathBoundsValidator {
    containment: Option<Arc<dyn Containment>>,
}

impl PathBoundsValidator {
    pub fn new(containment: Option<Arc<dyn Containment>>) -> Self {
        Self { containment }
    }
}

impl Validator for PathBoundsValidator {
    fn name(&self) -> &'static str {
        "path-bounds"
    }
    fn stage(&self) -> Stage {
        Stage::Detection
    }
    fn immunity(&self) -> Immunity {
        Immunity::ModeImmune
    }
    fn consent_overridable(&self) -> bool {
        true
    }
    fn check(&self, req: &ToolRequest<'_>, _ctx: &GateCtx<'_>) -> Option<Decision> {
        let containment = self.containment.as_ref()?;
        let root = containment.boundary_root()?;
        let grants = boundary_grants_for(
            req.tool_name,
            req.input,
            &root,
            &containment.boundary_dirs(),
            &containment.boundary_write_dirs(),
        );
        let authorized = authorized_dirs(&grants)?;
        if consent_allows(req, _ctx) {
            return None;
        }
        let detail = format!("Path is outside the workspace. Approving grants {authorized}.");
        Some(Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: self.name(),
            detail,
            containment_note: None,
        }))
    }
}

/// The directories an approval authorizes, as the card states them. Every
/// grant is named: the remember option installs them all, so a reason that
/// named one would describe less than the approval grants.
fn authorized_dirs(grants: &[BoundaryGrant]) -> Option<String> {
    let access = grants.first()?.access;
    let names: Vec<String> = grants
        .iter()
        .map(|grant| grant.directory.display().to_string())
        .collect();
    let joined = names.join(", ");
    Some(match (access, names.len()) {
        (BoundaryAccess::ReadOnly, 1) => format!("read-only directory {joined}"),
        (BoundaryAccess::ReadOnly, _) => format!("read-only directories {joined}"),
        (_, 1) => format!("parent directory {joined} with read-write access"),
        (_, _) => format!("parent directories {joined} with read-write access"),
    })
}

#[cfg(test)]
mod tests;
