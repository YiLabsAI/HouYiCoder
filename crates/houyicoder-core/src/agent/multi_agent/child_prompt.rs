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

use crate::agent::prompt::project_context_section;

/// Append the env details block to a child's base prompt: cwd, git-repo
/// presence, platform, and model. The current date is deliberately absent
/// here -- it rides the user context, which is rebuilt per run, so a child
/// spawned across a local-midnight boundary still sees the right day.
pub fn child_system_prompt(base_prompt: &str, cwd: &Path, model: &str) -> String {
    let mut text = String::new();
    if !base_prompt.is_empty() {
        text.push_str(base_prompt);
        text.push_str("\n\n");
    }
    text.push_str(&env_block(cwd, model));
    text
}

/// The child's user context: the project memory file content (AGENTS.md
/// equivalent + local overlay) plus the current date line. The omit flag
/// drops only the project memory -- the date survives, because a child that
/// reads or writes anything time-sensitive still needs it. Yields only the
/// date line when the flag is set or no memory file is found.
pub fn child_user_context(cwd: &Path, omit_project_context: bool) -> String {
    let project = if omit_project_context {
        None
    } else {
        project_context_section(cwd)
    };
    match project {
        Some(p) => format!("{p}\n\n{}", utc_date_line()),
        None => utc_date_line(),
    }
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
/// actually needs it.
fn env_block(cwd: &Path, model: &str) -> String {
    let repo = if is_git_workdir(cwd) { "Yes" } else { "No" };
    format!(
        "<env>\nWorking directory: {cwd}\nIs directory a git repo: {repo}\n\
         Platform: {platform}\nYou are powered by the model named {model}.\n</env>",
        cwd = cwd.display(),
        platform = std::env::consts::OS,
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

/// Today's date line. UTC, not local: the two differ only inside the
/// local-midnight-to-UTC-midnight window, immaterial for date-grounding a
/// coding run, and staying UTC keeps a timezone dependency out of this crate.
fn utc_date_line() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("Today's date is {}.", iso_date(secs))
}

fn iso_date(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since the Unix epoch to a civil (y, m, d) triple. The shifted-era
/// form keeps the arithmetic valid across the epoch boundary (negative day
/// counts included) without a calendar dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::apply_effort_settings;
    use houyicoder_protocol::llm::ModelSettings;
    use std::fs;
    use std::path::PathBuf;

    /// A per-process temp dir so parallel test runs do not collide.
    fn scratch_dir(label: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "child-prompt-test-{}-{}",
            label,
            std::process::id()
        ));
        fs::create_dir_all(&p).expect("mkdir scratch");
        p
    }

    #[test]
    fn test_env_block_appended_tail() {
        let dir = scratch_dir("tail");
        let p = child_system_prompt("Base instructions.", &dir, "qwen3-coder");
        assert!(p.starts_with("Base instructions."));
        assert!(p.ends_with("</env>"), "env block is the tail: {p}");
        assert!(p.contains("Working directory: "));
        assert!(p.contains("Platform: "));
        assert!(
            p.contains("You are powered by the model named qwen3-coder."),
            "{p}"
        );
    }

    #[test]
    fn test_git_repo_detected() {
        let dir = scratch_dir("git-yes");
        fs::create_dir_all(dir.join(".git")).expect("mkdir .git");
        let p = child_system_prompt("base", &dir, "m");
        assert!(p.contains("Is directory a git repo: Yes"), "{p}");
    }

    #[test]
    fn test_git_repo_walks_up() {
        let root = scratch_dir("git-walk");
        fs::create_dir_all(root.join(".git")).expect("mkdir .git");
        let sub = root.join("sub");
        fs::create_dir_all(&sub).expect("mkdir sub");
        let p = child_system_prompt("base", &sub, "m");
        assert!(p.contains("Is directory a git repo: Yes"), "{p}");
    }

    #[test]
    fn test_plain_dir_not_repo() {
        let dir = scratch_dir("git-no");
        let p = child_system_prompt("base", &dir, "m");
        assert!(p.contains("Is directory a git repo: No"), "{p}");
    }

    #[test]
    fn test_user_context_inherits_memory() {
        let dir = scratch_dir("ctx-inherit");
        fs::write(dir.join("AGENTS.md"), "project rules here").expect("write");
        let ctx = child_user_context(&dir, false);
        assert!(ctx.contains("project rules here"), "{ctx}");
        assert!(ctx.contains("Today's date is "), "{ctx}");
    }

    /// The omit flag drops only the project memory; the date survives,
    /// because a child that reads or writes anything time-sensitive still
    /// needs it.
    #[test]
    fn test_user_context_omit_memory() {
        let dir = scratch_dir("ctx-omit");
        fs::write(dir.join("AGENTS.md"), "secret project rules").expect("write");
        let ctx = child_user_context(&dir, true);
        assert!(!ctx.contains("secret project rules"), "{ctx}");
        assert!(ctx.contains("Today's date is "), "{ctx}");
    }

    #[test]
    fn test_user_context_date_only() {
        let dir = scratch_dir("ctx-none");
        let ctx = child_user_context(&dir, false);
        assert!(ctx.starts_with("Today's date is "), "{ctx}");
    }

    /// The cost gate: a child's requests carry no extended thinking. On the
    /// qwen dialect the lowest tier maps to thinking explicitly disabled.
    #[test]
    fn test_child_effort_disables_thinking() {
        let mut s = ModelSettings::default();
        apply_effort_settings(&mut s, "qwen3-coder", resolve_child_effort());
        assert_eq!(s.enable_thinking, Some(false));
        assert!(s.thinking_budget.is_none());
    }

    /// Reasoning dialects have no hard-off; the lowest tier is the closest
    /// available setting.
    #[test]
    fn test_child_effort_reasoning_low() {
        let mut s = ModelSettings::default();
        apply_effort_settings(&mut s, "o3-mini", resolve_child_effort());
        assert_eq!(s.reasoning_effort, Some(EffortLevel::Low));
    }

    #[test]
    fn test_civil_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    /// Leap-day boundaries: 2024-02-29 is day 19782 and 2024-03-01 is 19783.
    #[test]
    fn test_civil_leap_boundaries() {
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
    }

    #[test]
    fn test_iso_date_format() {
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_date(86_400), "1970-01-02");
    }
}
