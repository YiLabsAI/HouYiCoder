//! The houyi cleanup subcommand: review or apply the session prune plan.
//!
//! Split from main.rs on the per-file size gate. Default (dry-run) prints a
//! summary; --verbose lists every entry; --apply executes after a typed
//! confirmation, or non-interactively with --yes. No alternate screen here
//! (CLI subcommand, not TUI), so print is the correct sink. The summary
//! keeps a backlog that plan_all reports in the tens of thousands to a
//! screen, where the per-entry list would flood it.

use houyicoder_service::session_prune::{PruneEntry, PrunePlan};

pub(crate) fn run_cleanup(
    apply: bool,
    verbose: bool,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let sessions_root = houyicoder_service::composition::session_log_root();
    let shell_snapshots = houyicoder_config::config_home().join("shell-snapshots");
    let debug_log = std::env::current_dir()
        .unwrap_or_default()
        .join(".houyicoder")
        .join("debug.log");
    // No current session for a standalone cleanup invocation — the
    // protected set is just the lock-held sessions from the probe.
    let (policy, targets, _threshold) = crate::housekeeping::build_prune_context(
        &sessions_root,
        &shell_snapshots,
        Some(&debug_log),
        None,
    );
    let plan = houyicoder_service::session_prune::plan_all(&targets, &policy);
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
    let _guard = match crate::housekeeping::try_prune_lock() {
        Some(g) => g,
        None => {
            println!(
                "Another prune is running (background sweep or another process). Try again later."
            );
            return Ok(());
        }
    };
    // Each session is deleted while this process holds that session's lock,
    // so a session resumed between the plan above and the delete below is
    // held out instead of deleted underneath the resume.
    let (report, skipped) = crate::housekeeping::apply_prune_locked(&plan);
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
    use houyicoder_service::session_prune::{PruneKind, PruneReason};
    println!("{} entries prunable ({} kept).", plan.len(), plan.kept);
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
                println!("{kind:?} {reason:?} {n}");
            }
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
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
    use houyicoder_service::session_prune::{PruneKind, PruneReason};
    let kind = match entry.kind {
        PruneKind::Session => "session",
        PruneKind::Snapshot => "snapshot",
        PruneKind::DebugLog => "debug-log",
    };
    let reason = match entry.reason {
        PruneReason::Ttl => "ttl",
        PruneReason::EmptyTtl => "empty-ttl",
        PruneReason::CapOverflow => "cap-overflow",
    };
    let name = entry
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{kind:8} {reason:12} {name}")
}

/// The typed-confirmation gate for --apply. Reads stdin at the call site so
/// the pure judgment (confirm_granted) is unit-testable.
fn confirm_apply(n: usize) -> bool {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        println!("--apply needs --yes when stdin is not a terminal (non-interactive).");
        return false;
    }
    print!("About to remove {n} entries. Type 'yes' to proceed: ");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    confirm_granted(&line)
}

/// Only the full word "yes" grants. A backlog in the tens of thousands is an
/// irreversible delete a hand-slip away from "y", so the abbreviation is
/// rejected on purpose; "no" and a blank line refuse.
pub(crate) fn confirm_granted(answer: &str) -> bool {
    answer.trim() == "yes"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_confirm_granted_full_yes() {
        assert!(confirm_granted("yes"));
        assert!(confirm_granted("  yes  \n"));
    }

    #[test]
    fn test_confirm_refuses_non_yes() {
        assert!(!confirm_granted("Yes"), "case-sensitive: literal yes only");
        assert!(
            !confirm_granted("y"),
            "abbreviation is a hand-slip distance from a 46k delete"
        );
        assert!(!confirm_granted(""));
        assert!(!confirm_granted("no"));
        assert!(!confirm_granted("yes please"));
    }
}
