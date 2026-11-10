//! Bypass-immune safety checks. Certain paths are protected so strongly that
//! neither the current mode nor a stored consent can auto-allow a call that
//! touches them: the version-control directory, the control files under a
//! configuration directory, the instruction and startup files, and the memory
//! store. An instruction file or a memory store is only protected from being
//! rewritten, since a read of either decides nothing. The check is a coarse
//! text heuristic over the call's primary content string; a false negative is
//! caught by the protected-path resolved target and by the sandbox fence.

use serde_json::Value;

use houyicoder_api::sandbox::protected_path_hits;
use houyicoder_api::shell::heredoc::strip_quoted_heredoc_bodies;
use houyicoder_api::tool::is_shell_tool;

use crate::compound::{is_readonly_compound, split_compound};
use crate::rule::{Effect, input_content};

/// Whether a tool call touches a protected path. Returns Ask when the call's
/// primary content string names one of the bypass-immune paths; None
/// otherwise. It fires before the mode default and before a stored consent, so
/// neither the current mode nor an earlier answer silence it. A configuration
/// directory only protects the control paths under it, so session logs and
/// captured output do not escalate and a worktree checked out under it keeps
/// working.
pub fn safety_check(tool_name: &str, input: Option<&Value>) -> Option<Effect> {
    let content = input_content(tool_name, input);
    if content.is_empty() {
        return None;
    }
    let shell = is_shell_tool(tool_name);
    // Strip quoted heredoc bodies: a cat <<'EOF' body is literal text, so a
    // protected path (.git/, .bashrc, ...) appearing in it must not trip
    // this bypass-immune check. Unquoted heredoc bodies stay (bash expands
    // them).
    let content = if shell {
        strip_quoted_heredoc_bodies(&content)
    } else {
        content
    };
    let hits = protected_path_hits(&content);
    if hits.control || hits.startup {
        return Some(Effect::Ask);
    }
    // An instruction file or a memory store is protected from being
    // rewritten, which is how an approval would change every later decision.
    // Reading one decides nothing, so a command that only reads does not
    // escalate.
    if (hits.instruction || hits.memory) && !(shell && reads_only(&content)) {
        return Some(Effect::Ask);
    }
    None
}

/// Whether a shell command is one the read-only classifier statically
/// attests: no redirect, no substitution, no write option.
fn reads_only(content: &str) -> bool {
    let segments = split_compound(content);
    let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
    !refs.is_empty() && is_readonly_compound(&refs)
}

/// Whether a string names a protected path, case-insensitively. Split out
/// from safety_check so the same judgement can be applied to a path resolved
/// to the file it actually names, and to the segments of a shell command the
/// caller has already classified for reading.
pub(crate) fn marker_hit(content: &str) -> bool {
    protected_path_hits(content).any()
}

#[cfg(test)]
mod tests;
