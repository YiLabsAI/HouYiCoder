//! The houyi cleanup subcommand: review or apply the session prune plan.
//!
//! Split from main.rs on the per-file size gate. Default (dry-run) prints a
//! summary; --verbose lists every entry; --apply executes after a typed
//! confirmation, or non-interactively with --yes. No alternate screen here
//! (CLI subcommand, not TUI), so print is the correct sink. The summary
//! keeps a backlog that plan_all reports in the tens of thousands to a
//! screen, where the per-entry list would flood it.

use crate::session::housekeeping::apply_prune_locked;
use crate::session::housekeeping::build_prune_context;
use crate::session::housekeeping::try_prune_lock;
use houyicoder_service::composition::session_log_root;
use houyicoder_service::lifecycle::prune::plan_all;
use houyicoder_service::lifecycle::prune::{PruneEntry, PruneKind, PrunePlan, PruneReason};
use houyicoder_service::lifecycle::repair::{DescriptorRepair, repair_child_descriptors};
use std::env::current_dir;
use std::error::Error;
use std::io::IsTerminal;
use std::io::Write;
use std::io::stdin;
use std::io::stdout;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// What the apply path says about the repair it ran: nothing when the store
/// needed none, the count when some were written, and the failures alongside
/// so a store that keeps failing is not silent.
fn repair_summary(repair: DescriptorRepair) -> Option<String> {
    match (repair.written, repair.failed) {
        (0, 0) => None,
        (written, 0) => Some(format!("Repaired {written} child descriptors.")),
        (written, failed) => Some(format!(
            "Repaired {written} child descriptors, {failed} failed."
        )),
    }
}

/// One-word kind label shared by the summary and the per-entry list so the two
/// views never drift apart. Matches the file-system object being reclaimed.
fn kind_label(kind: PruneKind) -> &'static str {
    match kind {
        PruneKind::Session => "session",
        PruneKind::Snapshot => "snapshot",
        PruneKind::DebugLog => "debug-log",
    }
}

/// Why a prune entry was caught, as a short human-readable phrase. The enum
/// variants are rule names (TTL age, idle-empty net, count cap); the label is
/// what the user reads in the summary, so it states the cause, not the rule id.
fn reason_label(reason: PruneReason) -> &'static str {
    match reason {
        PruneReason::Ttl => "expired",
        PruneReason::EmptyTtl => "idle (empty)",
        PruneReason::CapOverflow => "over retention cap",
    }
}

/// One summary line: count + kind + cause. Extracted from the print loop so
/// the summary phrasing (indent, separator, label order) is assertable, not
/// just the labels it shares with the verbose entry formatter.
fn format_summary_count(n: usize, kind: PruneKind, reason: PruneReason) -> String {
    format!("  {n} {} · {}", kind_label(kind), reason_label(reason))
}

pub(crate) fn run_cleanup(apply: bool, verbose: bool, yes: bool) -> Result<(), Box<dyn Error>> {
    let sessions_root = session_log_root();
    let shell_snapshots = houyicoder_config::config_home().join("shell-snapshots");
    let debug_log = current_dir()
        .unwrap_or_default()
        .join(".houyicoder")
        .join("debug.log");
    // No current session for a standalone cleanup invocation — the
    // protected set is just the lock-held sessions from the probe.
    let (policy, targets, _threshold) = build_prune_context(
        &sessions_root,
        &shell_snapshots,
        Some(&debug_log),
        None,
        &houyicoder_config::config_home(),
    );
    let plan = plan_all(&targets, &policy);
    if plan.entries.is_empty() {
        println!("Nothing to prune.");
        return Ok(());
    }
    print_prune_summary(&plan);
    if verbose {
        for entry in &plan.entries {
            println!("{}", format_prune_entry(entry));
        }
    }
    if !apply {
        println!("Dry run -- pass --apply to execute.");
        return Ok(());
    }
    if !yes && !confirm_apply(plan.len()) {
        println!("Aborted.");
        return Ok(());
    }
    // Acquire the same .prune.lock the background sweep uses so the two
    // paths never delete concurrently. A held lock is not an error here --
    // the user re-runs when the sweep is done. The guard lives until the
    // end of this scope, so apply_prune runs under the lock.
    let _guard = match try_prune_lock(&houyicoder_config::config_home()) {
        Some(g) => g,
        None => {
            println!(
                "Another prune is running (background sweep or another process). Try again later."
            );
            return Ok(());
        }
    };
    // The repair is not bounded by the plan -- it heals the store -- but it
    // is the apply path's second act, so a store with nothing to prune keeps
    // its residue until the background sweep runs. It writes, so it stays
    // under the prune lock and behind the confirmation. It cannot move the
    // plan: a child is bounded by the same TTL whether its class came from
    // the descriptor or from its log.
    let repaired = repair_child_descriptors(&sessions_root);
    if let Some(line) = repair_summary(repaired) {
        println!("{line}");
    }
    // Each session is deleted while this process holds that session's lock,
    // so a session resumed between the plan above and the delete below is
    // held out instead of deleted underneath the resume.
    let (report, skipped) = apply_prune_locked(&plan);
    println!(
        "Removed {}, truncated {} logs, {} skipped (live), errors {}.",
        report.removed, report.truncated, skipped, report.errors
    );
    Ok(())
}

/// A one-screen summary of the prune plan: the total, a count per
/// (kind, reason) -- enough to recognize the shape of the backlog (all
/// empty-ttl, or a TTL wave, or cap overflow) -- and three oldest entries
/// as concrete samples. The per-entry list is --verbose.
fn print_prune_summary(plan: &PrunePlan) {
    println!(
        "{} entries prunable ({} user sessions kept).",
        plan.len(),
        plan.kept
    );
    for kind in [PruneKind::Session, PruneKind::Snapshot, PruneKind::DebugLog] {
        for reason in [
            PruneReason::Ttl,
            PruneReason::EmptyTtl,
            PruneReason::CapOverflow,
        ] {
            let n = plan
                .entries
                .iter()
                .filter(|e| e.kind == kind && e.reason == reason)
                .count();
            if n > 0 {
                println!("{}", format_summary_count(n, kind, reason));
            }
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut oldest: Vec<_> = plan.entries.iter().collect();
    oldest.sort_by_key(|e| e.last_active);
    let samples: Vec<String> = oldest
        .iter()
        .take(3)
        .map(|e| {
            let name = e
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let days = now.saturating_sub(e.last_active) / (24 * 3600);
            format!("{name} ({days}d)")
        })
        .collect();
    if !samples.is_empty() {
        let more = plan.len().saturating_sub(samples.len());
        println!("oldest: {}  +{more} more", samples.join(", "));
    }
    println!("run `houyi cleanup --apply` to remove.");
}

/// One prune entry as the --verbose per-line form.
fn format_prune_entry(entry: &PruneEntry) -> String {
    let kind = kind_label(entry.kind);
    let reason = reason_label(entry.reason);
    let name = entry
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{kind:10} {reason:20} {name}")
}

/// The typed-confirmation gate for --apply. Reads stdin at the call site so
/// the pure judgment (confirm_granted) is unit-testable.
fn confirm_apply(n: usize) -> bool {
    if !stdin().is_terminal() {
        println!("--apply needs --yes when stdin is not a terminal (non-interactive).");
        return false;
    }
    print!("About to remove {n} entries. Type y or yes to proceed: ");
    Write::flush(&mut stdout()).ok();
    let mut line = String::new();
    if stdin().read_line(&mut line).is_err() {
        return false;
    }
    confirm_granted(&line)
}

/// Accepts "y" or "yes" (case-insensitive). The --yes flag covers
/// non-interactive use; the interactive gate matches apt, npm, docker,
/// and git clean -i — all accept "y".
pub(crate) fn confirm_granted(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests;
