//! Child prompt assembly: the system prompt, user context, and effort gate a
//! spawned child carries, built from the resolved agent definition and the
//! spawn-time environment. Pure functions; the caller composes them into the
//! child Runner at spawn.
//!
//! Unlike the parent's prompt, the child's env details ride the system prompt
//! tail: a child session is short-lived and single-purpose, so the
//! byte-stable-prefix argument that keeps env out of the parent prompt does
//! not apply, and the child needs the ground truth of where it runs before
//! its first tool call.

use std::path::Path;

use houyicoder_protocol::llm::EffortLevel;

use crate::agent::prompt::date::utc_date_line;
use crate::agent::prompt::project_context_section;
use std::env::consts::OS;
#[cfg(test)]
use std::env::temp_dir;
#[cfg(test)]
use std::process::id;

/// Assemble a child's system prompt: its base instructions, the project
/// memory file (AGENTS.md equivalent plus the local overlay) unless the
/// definition omits it, then the env details block as the tail.
///
/// Host-injected context belongs here and not in the child's first user
/// message, matching where the parent carries it. A child's user message is
/// recorded as conversation and replayed into the parent's inline view, so
/// context injected there is shown to the user as if the delegation had
/// asked for it -- a whole memory file rendered above the actual task, and
/// identical for every child, which reads as the view repeating itself.
/// Keeping it in the system prompt also makes the cached prefix identical
/// across children of the same type in one repo.
pub fn child_system_prompt(
    base_prompt: &str,
    cwd: &Path,
    model: &str,
    omit_project_context: bool,
) -> String {
    let mut text = String::new();
    if !base_prompt.is_empty() {
        text.push_str(base_prompt);
        text.push_str("\n\n");
    }
    if !omit_project_context && let Some(project) = project_context_section(cwd) {
        text.push_str(&project);
        text.push_str("\n\n");
    }
    text.push_str(&env_block(cwd, model));
    text
}

/// The effort level a child's requests carry: the lowest tier. On thinking
/// dialects that maps to thinking disabled, so a fan-out of children does
/// not multiply reasoning-token spend. The one exception is the fork path,
/// which inherits the parent's effort to keep its request prefix identical
/// for cache hits.
pub fn resolve_child_effort() -> Option<EffortLevel> {
    Some(EffortLevel::Low)
}

/// The env details block. The git line is a presence flag, not a status
/// snapshot: a status read at spawn time would be stale by the time the
/// child's first turn runs, and the child can read status itself when a task
/// actually needs it. The date sits here with the rest of the run's ground
/// truth; a child is short-lived, so a spawn-time reading cannot go stale
/// within its own life.
fn env_block(cwd: &Path, model: &str) -> String {
    let repo = if is_git_workdir(cwd) { "Yes" } else { "No" };
    format!(
        "<env>\nWorking directory: {cwd}\nIs directory a git repo: {repo}\n\
         Platform: {platform}\n{date}\nYou are powered by the model named {model}.\n</env>",
        cwd = cwd.display(),
        platform = OS,
        date = utc_date_line(),
    )
}

/// Whether the cwd sits inside a git working tree: any ancestor directory
/// carrying a .git entry (a directory for a normal repo, a file for a linked
/// worktree). A plain existence walk, not a git invocation -- the child's
/// prompt assembly must not shell out.
fn is_git_workdir(cwd: &Path) -> bool {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return true;
        }
        dir = d.parent();
    }
    false
}

#[cfg(test)]
mod tests;
