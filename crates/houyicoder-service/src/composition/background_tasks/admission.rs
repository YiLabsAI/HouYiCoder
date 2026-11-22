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
        let record_sig = normalize_command(&record.command);
        let is_dup = !candidate_sig.is_empty() && record_sig == candidate_sig;
        if duplicate.is_none() && is_dup {
            duplicate = Some(summary.clone());
        }
        if build_lock_sibling.is_none()
            && !is_dup
            && let Some(ref domain) = candidate_lock
            && build_lock_domain(&record_sig).as_ref() == Some(domain)
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
/// invocations sharing the same target directory receive a non-blocking hint.
fn build_lock_domain(normalized: &str) -> Option<String> {
    let mut target_dir = "default";
    let mut uses_cargo_or_make = false;
    let mut saw_command_word = false;
    let mut skip_next = false;

    for tok in normalized.split_whitespace() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(tok, "&&" | "||" | ";" | "|") {
            saw_command_word = false;
            continue;
        }
        if !saw_command_word {
            if let Some(dir) = tok.strip_prefix("CARGO_TARGET_DIR=") {
                target_dir = dir;
                continue;
            }
            if tok.contains('=') {
                continue;
            }
            if tok == "cd" {
                skip_next = true;
                saw_command_word = true;
                continue;
            }
            if matches!(tok, "cargo" | "make") {
                uses_cargo_or_make = true;
            }
            saw_command_word = true;
        }
    }
    uses_cargo_or_make.then(|| target_dir.to_string())
}

/// Collapse whitespace and strip trailing file or stream redirects so two
/// runs of the same build or push redirected to different log files still
/// compare equal.
fn normalize_command(command: &str) -> String {
    let first = command.lines().next().unwrap_or_default().trim();
    let mut kept = Vec::new();
    let tokens: Vec<&str> = first.split_whitespace().collect();
    let mut idx = 0;
    while idx < tokens.len() {
        let tok = tokens[idx];
        if matches!(tok, ">" | ">>" | "<" | "1>" | "2>" | "&>") {
            idx += 2;
            continue;
        }
        if tok.starts_with('>')
            || tok.starts_with('<')
            || tok.starts_with("1>")
            || tok.starts_with("2>")
            || tok.starts_with("&>")
        {
            idx += 1;
            continue;
        }
        kept.push(tok);
        idx += 1;
    }
    kept.join(" ")
}
