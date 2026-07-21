//! The detection stage: deterministic heuristics that flag risky calls. These
//! are the bypass-immune safety checks that fire before the rule arms and the
//! mode default. Each validator wraps an existing predicate so the behavior is
//! preserved exactly; the validators are thin adapters over the shared
//! context, not a re-derivation of the rules.

use houyicoder_api::sandbox::{Containment, is_within_bounds, path_args_for_boundary};
use std::sync::Arc;

use crate::compound::compound_safe;
use crate::decision::{AskReason, AskSource, Decision};
use crate::git_discard::should_ask_before_git;
use crate::mode::ToolRequest;
use crate::pipeline::{GateCtx, Immunity, Stage, Validator, consent_allows};
use crate::rule::Effect;

/// rm / sudo / un-attestable redirects and substitution escalate to Ask even
/// when the mode default would Allow. Consent-overridable for an exact
/// pre-approved call.
fn should_ask_destructive(tool_name: &str, content: &str) -> bool {
    let lower = tool_name.to_ascii_lowercase();
    if !matches!(lower.as_str(), "bash" | "sh" | "exec" | "shell") {
        return false;
    }
    if content.is_empty() {
        return false;
    }
    // Scan interpreter inline code (bash -c "rm -rf x") like a direct command.
    let scan = interpreter_inline_code(content).unwrap_or(content);
    // Strip quoted heredoc bodies first: a cat <<'EOF' body is literal text
    // for cat, not a command, so a rm line inside it must not trip the
    // destructive-command scan. Unquoted heredoc bodies stay (bash expands
    // them, so a real command could hide there).
    let scan = crate::heredoc::strip_quoted_heredoc_bodies(scan);
    let lower_content = scan.to_ascii_lowercase();
    for word in lower_content.split(|c: char| !c.is_alphanumeric()) {
        // Deletion, privilege escalation, and content-overwrite commands. mv,
        // chmod -R, chown -R are deferred (broad false-positive risk; a
        // recursive/mass-change refinement lands separately).
        //
        // kill/pkill are intentionally NOT here: their effect (signaling
        // processes) is not a filesystem path operation, so the sandbox
        // fence cannot catch it. The destructive-command list here is a
        // permission gate, not informational, so adding process-signal
        // commands to it would gate them without a sandbox backstop. In
        // Auto mode kill/pkill fall through to the mode default (Allow) by
        // the user's explicit "don't ask" choice; in Manual/default they
        // Ask via bash's Execute side effect. This is a recorded decision,
        // not a gap — adding them here would ask for every backgrounded
        // process the agent spawns (high false-positive cost; the fence
        // cannot mitigate a signal anyway).
        if matches!(word, "rm" | "rmdir" | "unlink" | "sudo" | "dd" | "truncate") {
            return true;
        }
    }
    !crate::compound::is_attestable(content)
}

/// Detect network-egress commands in bash: curl, wget, git push, package
/// publish, scp, rsync, ssh. These carry external side effects (data leaves
/// the sandbox). The gate always asks for these — never a silent allow in
/// Auto, never a deny citing the fence. The fence blocks at execution time
/// when it does not permit egress, and post_transform attaches that verdict
/// as a containment_note so the user knows approval may not help.
fn should_ask_egress(tool_name: &str, content: &str) -> bool {
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
    // Split into segments by shell separators (&&, ;, |, newline), then
    // take the first token of each segment — that's the command position.
    // This avoids matching egress tool names inside arguments/echo/man/which.
    for segment in lower_content.split(['&', ';', '|', '\n']) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        // Skip leading env-var assignments (FOO=bar) to find the command.
        // X=1 curl evil.com → skip "x=1", take "curl".
        // Collect non-assignment tokens so the subcommand position is
        // correct (nth(1) from the filtered list, not from the original
        // which counts env-var assignments).
        let tokens: Vec<&str> = segment
            .split_whitespace()
            .filter(|t| !t.contains('='))
            .collect();
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
pub struct DestructiveCommandValidator;

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
/// by the session consent, or by the toggle. Stays as one validator this
/// sprint; a later sprint moves the checkpoint arms into builtin rules and
/// keeps only the discard detection here.
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
        let git_word = should_ask_before_git(req.tool_name, ctx.content)?;
        // An allow rule (including a session-scope "don't ask again" allow
        // rule the user consented to) suppresses the checkpoint: the
        // pre-computed effect is Allow, so the call does not escalate here.
        // The direct commit / rebase / reset / tag forms are caught earlier
        // by the builtin ask rules at UserAsk; this validator reaches only
        // the wrapped-interpreter forms (bash -c "git commit") the prefix
        // rules cannot match, plus the discard forms (force push, clean -fd).
        if ctx.effect == Some(Effect::Allow) {
            return None;
        }
        // The /permission git toggle disables the checkpoint ops (commit /
        // rebase / reset / tag) together — both the builtin ask rule (direct
        // form) and this detection arm (wrapped form). The discard forms
        // (force push, clean, stash drop, ...) are NOT toggle-gated: they
        // stay asks regardless.
        if matches!(git_word, "commit" | "rebase" | "reset" | "tag") && !ctx.git_checkpoint_enabled
        {
            return None;
        }
        if consent_allows(req, ctx) {
            return None;
        }
        Some(Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: self.name(),
            detail: "git checkpoint op needs confirmation".into(),
            containment_note: None,
        }))
    }
}

/// Network egress: curl, wget, git push, package publish, scp, rsync, ssh.
/// Always asks. The fence is the single authority over containment: when it
/// does not permit egress the kernel refuses at execution time, and
/// post_transform attaches the fence's verdict (via would_block) as a
/// containment_note so the user sees that approval will not help. The gate
/// never denies on containment grounds — that would make it a second
/// authority beside the fence.
pub struct NetworkEgressValidator;

/// The stable name of the network-egress validator. A metrics bucket key and
/// the recognizer post_transform uses to query the fence at the network layer
/// (an egress call runs via bash so its side-effect classification is Exec,
/// but the fence blocks egress, not exec).
pub(crate) const NETWORK_EGRESS: &str = "network_egress";

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
        false
    }
    fn check(&self, req: &ToolRequest<'_>, ctx: &GateCtx<'_>) -> Option<Decision> {
        if !should_ask_egress(req.tool_name, ctx.content) {
            return None;
        }
        // The gate always asks for egress, never denies citing the fence.
        // The fence blocks at execution time; post_transform fills the note
        // (via would_block) when the fence is expected to reject, so the user
        // knows approval may not help.
        Some(Decision::Ask(AskReason {
            source: AskSource::Detection,
            validator: self.name(),
            detail: "this command reaches the network; approving runs it \
                     (the sandbox fence may still block it)"
                .into(),
            containment_note: None,
        }))
    }
}

/// A compound shell command that is not statically attestable. Any
/// un-attestable segment (redirect, command substitution, heredoc) escalates
/// the whole command to Ask. Consent-overridable for an exact pre-approved
/// call. Uses the pre-tokenized segments from the shared context so the
/// ladder tokenizes the command once.
pub struct CompoundCommandValidator;

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
        if ctx.segments.len() <= 1 {
            return None;
        }
        let refs: Vec<&str> = ctx.segments.iter().map(|s| s.as_str()).collect();
        if compound_safe(&refs) {
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

/// A grep/glob path whose canonical form the fence says is outside the
/// workspace + authorized dirs surfaces an Ask so the user can grant it —
/// instead of the tool's hard PathEscapes rejection. Bypass-immune safety
/// (ModeImmune): fires in Auto like destructive/egress, the agent cannot
/// auto-allow an out-of-workspace read. The gate only asks; it does not judge
/// in-bounds (an uncertain canonicalize or a missing fence degrades to None,
/// letting confine_path / the kernel fence enforce). Holds the containment
/// handle directly (not via GateCtx — the design keeps GateCtx fence-free); the
/// gate injects it when with_containment wires the fence. deny-wins is
/// preserved: RuleDeny fires earlier in the ladder, so a Deny grep rule blocks
/// before this asks.
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
        let additional = containment.boundary_dirs();
        for p in path_args_for_boundary(req.tool_name, req.input) {
            let candidate = root.join(&p);
            if let Ok(canonical) = std::fs::canonicalize(&candidate)
                && !is_within_bounds(&canonical, &root, &additional)
            {
                if consent_allows(req, _ctx) {
                    return None;
                }
                return Some(Decision::Ask(AskReason {
                    source: AskSource::Detection,
                    validator: self.name(),
                    detail: "path outside the workspace and authorized dirs".into(),
                    containment_note: None,
                }));
            }
        }
        None
    }
}
