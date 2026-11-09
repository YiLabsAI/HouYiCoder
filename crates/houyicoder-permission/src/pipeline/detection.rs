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

use super::reasons::{compound_unverified_step, describe_destructive, describe_egress_reason};
use crate::compound::{
    compound_safe, is_effect_safe, is_readonly_compound, removes_only_temp, split_compound,
};
use crate::decision::{AskReason, AskSource, Decision};
use crate::egress::{egress_outputs_safe, egress_scope_list};
use crate::git_safety::{collect_git_ask_ops, decode_token, is_shell_control_keyword};
use crate::heredoc::strip_quoted_heredoc_bodies;
use crate::mode::{PermissionMode, ToolRequest};
use crate::pipeline::{GateCtx, Immunity, Stage, Validator, consent_allows};
use crate::rule::{Effect, RuleContent, evaluate};

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

/// Destructive verbs (rm, sudo, dd, …) escalate to Ask even when the mode
/// default would Allow. Consent-overridable for an exact pre-approved call.
/// Un-attestable constructs (redirect, substitution) are escalated by the
/// compound validator, not here: destructive means a destructive verb.
fn should_ask_destructive(tool_name: &str, content: &str) -> bool {
    if !is_shell_tool(tool_name) {
        return false;
    }
    if content.is_empty() {
        return false;
    }
    let expanded = expand_inline_interpreter(content);
    // Strip quoted heredoc bodies first: a cat <<'EOF' body is literal text,
    // so a rm line inside it must not trip the scan. Unquoted heredoc bodies
    // stay (bash expands them, so a real command could hide there).
    let scan = strip_quoted_heredoc_bodies(&expanded);
    let unquoted: String = scan
        .chars()
        .filter(|&c| c != '\'' && c != '"' && c != '\\')
        .collect();
    // An underscore joins a word: a search pattern such as rm_targets_are_temp
    // is one identifier, not the verb rm followed by arguments. Splitting on
    // the underscore read a read-only search for that name as the deletion it
    // was looking for, so the gate asked about a call that could not delete
    // anything, and a child agent waiting on that card stalled until its
    // ceiling. Hyphens stay separators: a real invocation may be written
    // without spaces only across a word boundary, never inside a verb.
    for text in [scan.as_str(), unquoted.as_str()] {
        let lower_content = text.to_ascii_lowercase();
        for word in lower_content.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if matches!(word, "rm" | "rmdir" | "unlink" | "sudo" | "dd" | "truncate") {
                return true;
            }
        }
    }
    false
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
    // Include interpreter inline code alongside the outer command so neither a
    // wrapped egress call nor a trailing command after an interpreter escapes
    // the scan.
    let expanded = expand_inline_interpreter(content);
    let lower_content = expanded.to_ascii_lowercase();
    // Split into segments by shell separators and substitution delimiters,
    // then take the first non-assignment token of each segment.
    for segment in lower_content.split(['&', ';', '|', '\n', '(', ')', '`']) {
        let segment = segment.trim().trim_start_matches('$');
        if segment.is_empty() {
            continue;
        }
        // Skip leading env-var assignments (FOO=bar) to find the command.
        // X=1 curl evil.com → skip "x=1", take "curl".
        // Collect non-assignment tokens so the subcommand position is
        // correct (nth(1) from the filtered list, not from the original
        // which counts env-var assignments).
        let decoded_tokens: Vec<String> = segment
            .split_whitespace()
            .map(decode_token)
            .filter(|t| {
                !t.is_empty()
                    && (!t.contains('=') || t.starts_with('-'))
                    && !is_shell_control_keyword(t)
            })
            .collect();
        let tokens: Vec<&str> = decoded_tokens.iter().map(String::as_str).collect();
        let Some(&cmd) = tokens.first() else { continue };
        let sub = strip_quotes(tokens.get(1).copied().unwrap_or(""));
        // Direct egress tools.
        let egress_tools = [
            "curl", "wget", "httpie", "scp", "rsync", "ssh", "sftp", "ftp", "nc", "netcat",
            "telnet",
        ];
        if egress_tools.contains(&cmd) {
            return true;
        }
        // GitHub CLI reaches the network unless the call only reads. The read
        // set is the inspection verbs plus a bare api request, which posts only
        // when a method or a field says so.
        if cmd == "gh" && !gh_reads_only(&tokens[1..]) {
            return true;
        }
        // A fetch or a remote listing against this repository's own configured
        // remotes only moves local refs: nothing of the user's leaves the
        // machine and no working file changes. It is the same traffic as a
        // package fetch, so it asks only when it names a source outside the
        // repository's configuration.
        if cmd == "git" && matches!(sub, "fetch" | "ls-remote") {
            if !git_egress_names_external_source(&tokens[1..]) {
                continue;
            }
            return true;
        }
        // git push / clone / pull — these send local work out, or bring a tree
        // in and check it out, so they stay behind the ask.
        if cmd == "git" && matches!(sub, "push" | "clone" | "pull") {
            return true;
        }
        // Package publish: npm publish, cargo publish, pip upload.
        if (cmd == "npm" || cmd == "cargo") && sub == "publish" {
            return true;
        }
        if cmd == "pip" && sub == "upload" {
            return true;
        }
    }
    false
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
    let lower = content.to_ascii_lowercase();
    for segment in lower.split(['&', ';', '|', '\n', '(', ')', '`']) {
        let decoded: Vec<String> = segment
            .split_whitespace()
            .map(decode_token)
            .filter(|t| !t.is_empty() && !t.contains('=') && !is_shell_control_keyword(t))
            .collect();
        if decoded.first().map(|t| strip_quotes(t)) != Some("git") {
            continue;
        }
        let Some(sub) = decoded.get(1).map(|t| strip_quotes(t)) else {
            continue;
        };
        if let Some(op) = GIT_NETWORK_OPS.iter().find(|op| **op == sub) {
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
    for segment in lower.split(['&', ';', '|', '\n', '(', ')', '`']) {
        // Every word is kept here, including the flag=value form: the check is
        // about where the call points, and dropping the value would hide the
        // one argument that says so.
        let words: Vec<String> = segment
            .split_whitespace()
            .map(decode_token)
            .filter(|t| !t.is_empty() && !is_shell_control_keyword(t))
            .collect();
        if words.first().map(|t| strip_quotes(t)) != Some("git") {
            continue;
        }
        let args: Vec<&str> = words[1..].iter().map(|t| strip_quotes(t)).collect();
        if git_egress_names_external_source(&args) {
            return true;
        }
    }
    false
}

fn git_egress_names_external_source(args: &[&str]) -> bool {
    args.iter().any(|arg| {
        let arg = strip_quotes(arg);
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

/// Append the inline code of any interpreter segment onto the command so the
/// caller scans both the outer segments and the unwrapped script bodies.
/// Replacing the outer command with the first interpreter's inline code would
/// drop every segment that followed the interpreter.
pub(crate) fn expand_inline_interpreter(content: &str) -> String {
    let mut out = content.to_string();
    for segment in split_compound(content) {
        if let Some(inline) = interpreter_inline_code(segment.trim()) {
            out.push_str("; ");
            out.push_str(inline);
        }
    }
    out
}

/// If the content is an interpreter invocation carrying inline code, return the
/// inline code so the caller scans it like a command. Catches bash -c,
/// python -c, node -e, perl -e, ruby -e with an optional env wrapper. The
/// inline code is extracted as a shell argument: a quoted string keeps its
/// internal spaces (bash -c "curl http://x" yields "curl http://x"), a bare
/// word stops at the next whitespace. Returns None for script-file
/// invocations (bash script.sh). One level of unwrapping; nested
/// interpreter-inside-interpreter is not chased. Shared with the git-discard
/// classifier.
pub(crate) fn interpreter_inline_code(content: &str) -> Option<&str> {
    let mut tokens = content.split_whitespace();
    let first = tokens.next()?.to_ascii_lowercase();
    let cmd = if first == "env" {
        tokens.next()?.to_ascii_lowercase()
    } else {
        first
    };
    let flag = match cmd.as_str() {
        "bash" | "sh" | "dash" | "zsh" | "ksh" | "fish" | "python" | "python2" | "python3" => "-c",
        "node" | "perl" | "ruby" => "-e",
        _ => return None,
    };
    // Locate the flag in the raw content (not the whitespace-split view) so
    // a quoted inline argument keeps its internal spaces.
    let flag_pos = content.find(flag)?;
    let after = content[flag_pos + flag.len()..].trim_start();
    if after.is_empty() {
        return None;
    }
    let quote = after.as_bytes()[0];
    if quote == b'"' || quote == b'\'' {
        // A backslash escapes the next character inside double quotes, so a
        // closing quote that follows one is part of the code rather than its
        // end. Reading the first match instead would cut the body short and
        // hand the caller a prefix that looks clean.
        let body = &after[1..];
        let mut close = None;
        let mut chars = body.char_indices();
        while let Some((at, c)) = chars.next() {
            if quote == b'"' && c == '\\' {
                chars.next();
                continue;
            }
            if c as u8 == quote {
                close = Some(at);
                break;
            }
        }
        Some(&body[..close?])
    } else {
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        Some(&after[..end])
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
            && interpreter_inline_code(ctx.content).is_none()
            && removes_only_temp(ctx.content, writes_confined(&self.containment))
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
    let mut segments = ctx
        .content
        .split(['&', ';', '|', '\n', '(', ')', '`'])
        .filter(|part| !part.trim().is_empty());
    let Some(only) = segments.next() else {
        return false;
    };
    if segments.next().is_some() {
        return false;
    }
    let Some(op) = egress_remote_op(only) else {
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
