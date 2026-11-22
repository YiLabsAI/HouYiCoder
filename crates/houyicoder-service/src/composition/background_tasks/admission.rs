//! Active-slot capacity checks, duplicate detection, and build-lock hints
//! for background shell tasks.

use std::time::Instant;

use houyicoder_api::background::{BackgroundRefusal, RunningTaskSummary};

use super::TaskRecord;

pub(super) struct LaunchAdvisories {
    pub duplicate: Option<RunningTaskSummary>,
    pub build_lock_sibling: Option<RunningTaskSummary>,
}

/// Evaluate whether a command can enter the active background table, returning
/// duplicate or same-target build-lock advisories when admitted.
pub(super) fn judge_launch(
    tasks: &[TaskRecord],
    limit: usize,
    command: &str,
    reject_duplicate: bool,
) -> Result<LaunchAdvisories, BackgroundRefusal> {
    let now = Instant::now();
    let candidate_sig = normalize_command(command);
    let candidate_lock = build_lock_domain(&candidate_sig);
    let mut running = Vec::new();
    let mut duplicate = None;
    let mut build_lock_sibling = None;

    for record in tasks.iter().filter(|task| !task.status.is_terminal()) {
        let summary = RunningTaskSummary {
            id: record.id,
            elapsed_secs: now.saturating_duration_since(record.started).as_secs(),
            command: record.command.clone(),
        };
        let is_dup = !candidate_sig.is_empty() && record.signature == candidate_sig;
        if duplicate.is_none() && is_dup {
            duplicate = Some(summary.clone());
        }
        if build_lock_sibling.is_none()
            && !is_dup
            && let Some(ref domain) = candidate_lock
            && build_lock_domain(&record.signature).as_ref() == Some(domain)
        {
            build_lock_sibling = Some(summary.clone());
        }
        running.push(summary);
    }

    if reject_duplicate && let Some(existing) = duplicate {
        return Err(BackgroundRefusal::DuplicateCommand { existing });
    }
    if running.len() >= limit {
        return Err(BackgroundRefusal::CapacityFull { limit, running });
    }
    Ok(LaunchAdvisories {
        duplicate,
        build_lock_sibling,
    })
}

/// Identify the Cargo target lock domain for build/test commands so distinct
/// invocations sharing the same workspace or target directory receive a
/// non-blocking hint.
fn build_lock_domain(normalized: &str) -> Option<String> {
    let mut target_dir: Option<&str> = None;
    let mut cwd: Option<&str> = None;
    let mut uses_cargo_or_make = false;

    for line in normalized.lines() {
        let mut saw_command_word = false;
        let mut expect_cd_dir = false;
        for tok in line.split_whitespace() {
            if expect_cd_dir {
                expect_cd_dir = false;
                if !tok.starts_with('-') {
                    cwd = Some(tok);
                }
                continue;
            }
            if matches!(tok, "&&" | "||" | ";" | "|") {
                saw_command_word = false;
                continue;
            }
            if !saw_command_word {
                if let Some(dir) = tok.strip_prefix("CARGO_TARGET_DIR=") {
                    target_dir = Some(dir);
                    continue;
                }
                if tok.contains('=') {
                    continue;
                }
                if tok == "cd" {
                    expect_cd_dir = true;
                    saw_command_word = true;
                    continue;
                }
                if matches!(tok, "cargo" | "make") {
                    uses_cargo_or_make = true;
                }
                saw_command_word = true;
            }
        }
    }
    if !uses_cargo_or_make {
        return None;
    }
    Some(match target_dir {
        Some(dir) => format!("target:{dir}"),
        None => format!("cwd:{}", cwd.unwrap_or(".")),
    })
}

/// Collapse whitespace across all lines and strip output redirects (plus
/// null-stdin guards) so identical commands logged to different files match
/// while distinct lines or file inputs stay separate.
pub(super) fn normalize_command(command: &str) -> String {
    let mut normalized_lines = Vec::new();
    for raw_line in command.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        let mut kept = Vec::new();
        let mut idx = 0;
        while idx < tokens.len() {
            let tok = tokens[idx];
            if is_output_redirect_op(tok) {
                if tokens
                    .get(idx + 1)
                    .is_some_and(|next| !matches!(*next, "&&" | "||" | ";" | "|"))
                {
                    idx += 2;
                } else {
                    idx += 1;
                }
                continue;
            }
            if is_attached_output_redirect(tok) {
                idx += 1;
                continue;
            }
            if matches!(tok, "<" | "0<")
                && tokens.get(idx + 1).is_some_and(|next| is_null_device(next))
            {
                idx += 2;
                continue;
            }
            if tok
                .strip_prefix("0<")
                .or_else(|| tok.strip_prefix('<'))
                .is_some_and(is_null_device)
            {
                idx += 1;
                continue;
            }
            kept.push(tok);
            idx += 1;
        }
        if !kept.is_empty() {
            normalized_lines.push(kept.join(" "));
        }
    }
    normalized_lines.join("\n")
}

fn is_output_redirect_op(tok: &str) -> bool {
    matches!(tok, ">" | ">>" | "1>" | "1>>" | "2>" | "2>>" | "&>" | "&>>")
}

fn is_attached_output_redirect(tok: &str) -> bool {
    tok.starts_with('>') || tok.starts_with("1>") || tok.starts_with("2>") || tok.starts_with("&>")
}

fn is_null_device(target: &str) -> bool {
    target == "/dev/null" || target.eq_ignore_ascii_case("nul")
}
