//! The detection stage: deterministic heuristics that flag risky calls. These
//! are the bypass-immune safety checks that fire before the rule arms and the
//! mode default. Each validator wraps an existing predicate so the behavior is
//! preserved exactly; the validators are thin adapters over the shared
//! context, not a re-derivation of the rules.

use houyicoder_api::sandbox::{BoundaryAccess, Containment, Coverage, boundary_grants_for};
use std::path::PathBuf;
use std::sync::Arc;

use crate::compound::{compound_safe, is_effect_safe, is_readonly_compound, removes_only_temp};
use crate::decision::{AskReason, AskSource, Decision};
use crate::egress::{egress_destination, egress_outputs_safe, egress_scope_list};
use crate::git_discard::{collect_git_ask_ops, decode_token, is_shell_control_keyword};
use crate::heredoc::strip_quoted_heredoc_bodies;
use crate::mode::{PermissionMode, ToolRequest};
use crate::pipeline::{GateCtx, Immunity, Stage, Validator, consent_allows};
use crate::rule::{Effect, evaluate};

fn session_temp_dir(containment: &Option<Arc<dyn Containment>>) -> Option<PathBuf> {
    let c = containment.as_ref()?;
    if matches!(c.coverage(), Coverage::Fenced { .. }) {
        c.temp_dir()
    } else {
        None
    }
}

/// Destructive verbs (rm, sudo, dd, …) escalate to Ask even when the mode
/// default would Allow. Consent-overridable for an exact pre-approved call.
/// Un-attestable constructs (redirect, substitution) are escalated by the
/// compound validator, not here: destructive means a destructive verb.
fn should_ask_destructive(tool_name: &str, content: &str) -> bool {
    let lower = tool_name.to_ascii_lowercase();
    if !matches!(lower.as_str(), "bash" | "sh" | "exec" | "shell") {
        return false;
    }
    if content.is_empty() {
        return false;
    }
    let scan = interpreter_inline_code(content).unwrap_or(content);
    // Strip quoted heredoc bodies first: a cat <<'EOF' body is literal text,
    // so a rm line inside it must not trip the scan. Unquoted heredoc bodies
    // stay (bash expands them, so a real command could hide there).
    let scan = strip_quoted_heredoc_bodies(scan);
    let unquoted: String = scan
        .chars()
        .filter(|&c| c != '\'' && c != '"' && c != '\\')
        .collect();
    for text in [scan.as_str(), unquoted.as_str()] {
        let lower_content = text.to_ascii_lowercase();
        for word in lower_content.split(|c: char| !c.is_alphanumeric()) {
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
    let lower = tool_name.to_ascii_lowercase();
    if !matches!(lower.as_str(), "bash" | "sh" | "exec" | "shell") {
        return false;
    }
    if content.is_empty() {
        return false;
    }
    // Unwrap interpreter inline code (bash -c "curl x" -> scan "curl x") so
    // an egress tool wrapped in an interpreter is still detected.
    let scan = interpreter_inline_code(content).unwrap_or(content);
    let lower_content = scan.to_ascii_lowercase();
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
            .filter(|t| !t.is_empty() && !t.contains('=') && !is_shell_control_keyword(t))
            .collect();
        let tokens: Vec<&str> = decoded_tokens.iter().map(String::as_str).collect();
        let Some(&cmd) = tokens.first() else { continue };
        let sub = strip_quotes(tokens.get(1).copied().unwrap_or(""));
        let sub2 = strip_quotes(tokens.get(2).copied().unwrap_or(""));
        // Direct egress tools.
        let egress_tools = [
            "curl", "wget", "httpie", "scp", "rsync", "ssh", "sftp", "ftp", "nc", "netcat",
            "telnet",
        ];
        if egress_tools.contains(&cmd) {
            return true;
        }
        // GitHub CLI reaches the network unless it is a read-only view/list/diff/checks/status call.
        if cmd == "gh" {
            let readonly_gh = matches!(sub, "version" | "help")
                || (matches!(sub, "pr" | "issue" | "repo" | "release" | "run")
                    && matches!(sub2, "view" | "list" | "diff" | "checks" | "status"));
            if !readonly_gh {
                return true;
            }
        }
        // git push / clone / fetch / pull / ls-remote — network git ops.
        if cmd == "git" && matches!(sub, "push" | "clone" | "fetch" | "pull" | "ls-remote") {
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
        let close = after[1..].find(quote as char)?;
        Some(&after[1..1 + close])
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
        let temp_dir = session_temp_dir(&self.containment);
        if ctx.mode == PermissionMode::Auto
            && interpreter_inline_code(ctx.content).is_none()
            && removes_only_temp(ctx.content, temp_dir.as_deref())
        {
            return None;
        }
        if consent_allows(req, ctx) {
            return None;
        }
        Some(Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: self.name(),
            detail: "destructive pattern needs confirmation".into(),
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
                    format!("git {git_word} checkpoint op needs confirmation")
                }
                other => format!("git {other} discard op needs confirmation"),
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
        let temp_dir = session_temp_dir(&self.containment);
        if egress_outputs_safe(ctx.content, temp_dir.as_deref())
            && let Some(scopes) = egress_scope_list(ctx.content)
            && !scopes.is_empty()
            && scopes
                .iter()
                .all(|dest| ctx.egress_allow.iter().any(|allowed| allowed == dest))
        {
            return None;
        }
        if let Some(destination) = egress_destination(ctx.content)
            && ctx
                .egress_allow
                .iter()
                .any(|allowed| allowed == &format!("session:{destination}"))
        {
            return None;
        }
        if consent_allows(req, ctx) {
            return None;
        }
        Some(Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: self.name(),
            detail: "this command reaches the network".into(),
            containment_note: None,
        }))
    }
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
        let lower = req.tool_name.to_ascii_lowercase();
        if !matches!(lower.as_str(), "bash" | "sh" | "exec" | "shell") {
            return None;
        }
        let refs: Vec<&str> = ctx.segments.iter().map(|s| s.as_str()).collect();
        if refs.is_empty() {
            return None;
        }
        let temp_dir = session_temp_dir(&self.containment);
        if compound_safe(&refs)
            || is_readonly_compound(&refs)
            || (ctx.mode == PermissionMode::Auto
                && is_effect_safe(ctx.content, temp_dir.as_deref()))
        {
            return None;
        }
        if consent_allows(req, ctx) {
            return None;
        }
        Some(Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: self.name(),
            detail: "compound command needs confirmation".into(),
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
        let grant = grants.first()?;
        if consent_allows(req, _ctx) {
            return None;
        }
        let detail = match grant.access {
            BoundaryAccess::ReadOnly => format!(
                "path outside the workspace; approval authorizes read-only directory {}; choosing always persists it",
                grant.directory.display()
            ),
            BoundaryAccess::ReadWrite => format!(
                "path outside the workspace; approval authorizes parent directory {} with read-write access; choosing always persists it",
                grant.directory.display()
            ),
        };
        Some(Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: self.name(),
            detail,
            containment_note: None,
        }))
    }
}
