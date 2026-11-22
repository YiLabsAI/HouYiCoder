//! Active-slot capacity checks and duplicate-command detection for background
//! shell tasks.

use std::time::Instant;

use houyicoder_api::background::{BackgroundRefusal, RunningTaskSummary};

use super::TaskRecord;

/// Evaluate whether a command can enter the active background table, returning
/// an active duplicate summary when automatic handover allows a matching sibling.
pub(super) fn judge_launch(
    tasks: &[TaskRecord],
    limit: usize,
    command: &str,
    reject_duplicate: bool,
) -> Result<Option<RunningTaskSummary>, BackgroundRefusal> {
    let now = Instant::now();
    let candidate_sig = normalize_command(command);
    let mut running = Vec::new();
    let mut duplicate = None;

    for record in tasks.iter().filter(|task| !task.status.is_terminal()) {
        let summary = RunningTaskSummary {
            id: record.id,
            elapsed_secs: now.saturating_duration_since(record.started).as_secs(),
            command: record.command.clone(),
        };
        if duplicate.is_none()
            && !candidate_sig.is_empty()
            && normalize_command(&record.command) == candidate_sig
        {
            duplicate = Some(summary.clone());
        }
        running.push(summary);
    }

    if reject_duplicate && let Some(existing) = duplicate {
        return Err(BackgroundRefusal::DuplicateCommand { existing });
    }
    if running.len() >= limit {
        return Err(BackgroundRefusal::CapacityFull { limit, running });
    }
    Ok(duplicate)
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
