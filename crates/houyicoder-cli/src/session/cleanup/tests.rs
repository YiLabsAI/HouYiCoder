use super::*;
use houyicoder_service::lifecycle::prune::{PruneAction, PruneEntry, PruneKind, PruneReason};
use houyicoder_service::lifecycle::reclaim::ReclaimError;
use std::env::temp_dir;
use std::fs;
use std::fs::OpenOptions;
use std::process::id;
use std::time::Duration;

/// The apply path reports the blocks it freed, so a user who ran the cleanup
/// for disk space sees the disk come back instead of a silent zero.
#[test]
fn test_reclaim_summary_reports() {
    let root = temp_dir().join(format!("houyicoder_cleanup_reclaim_{}", id()));
    let shard = root.join(".cas").join("ab");
    fs::create_dir_all(&shard).expect("create shard");
    let stale = shard.join("ab00.bin.tmp");
    fs::write(&stale, b"never renamed").expect("write the interrupted temp");
    let handle = OpenOptions::new()
        .write(true)
        .open(&stale)
        .expect("open the temp");
    handle
        .set_modified(SystemTime::now() - Duration::from_secs(7200))
        .expect("age the temp");

    assert_eq!(
        reclaim_summary(&root),
        "Reclaimed 1 block.",
        "one block reads as a noun that agrees with the count"
    );
    assert!(!stale.exists(), "the temp is gone");
    fs::remove_dir_all(&root).ok();
}

/// Every ending of a pass reaches the operator in words: a partial pass says
/// the rest waits for the next run, a held lock says nothing ran, and a
/// stopped pass says what it freed before it stopped.
#[test]
fn test_reclaim_lines_read() {
    let freed = ReclaimReport {
        removed: 2,
        ..ReclaimReport::default()
    };
    assert_eq!(
        reclaim_line(ReclaimOutcome::Completed(freed)),
        "Reclaimed 2 blocks."
    );
    assert_eq!(
        reclaim_line(ReclaimOutcome::Partial(freed)),
        "Reclaimed 2 blocks; the pass stopped at its budget and the rest waits for the next run."
    );
    assert_eq!(
        reclaim_line(ReclaimOutcome::Skipped),
        "Block reclaim skipped: another pass holds its lock."
    );
    let stopped = ReclaimOutcome::Stopped {
        error: ReclaimError::Busy,
        report: ReclaimReport {
            removed: 1,
            ..ReclaimReport::default()
        },
    };
    assert_eq!(
        reclaim_line(stopped),
        "Block reclaim stopped after 1 block: another pass holds the reclaim lock"
    );
}

/// An empty plan is not an empty store: with --apply the manual lever still
/// reclaims, and a dry run leaves the store alone.
#[test]
fn test_empty_plan_reclaims() {
    let root = temp_dir().join(format!("houyicoder_cleanup_empty_{}", id()));
    let shard = root.join(".cas").join("ab");
    fs::create_dir_all(&shard).expect("create shard");
    let stale = shard.join("ab11.bin.tmp");
    fs::write(&stale, b"never renamed").expect("write the interrupted temp");
    let handle = OpenOptions::new()
        .write(true)
        .open(&stale)
        .expect("open the temp");
    handle
        .set_modified(SystemTime::now() - Duration::from_secs(7200))
        .expect("age the temp");

    assert_eq!(
        empty_plan_summary(false, &root),
        None,
        "a dry run reclaims nothing"
    );
    assert!(stale.exists(), "the temp is untouched");
    assert_eq!(
        empty_plan_summary(true, &root).as_deref(),
        Some("Reclaimed 1 block."),
        "the apply path reclaims with no plan"
    );
    assert!(!stale.exists(), "the temp is gone");
    fs::remove_dir_all(&root).ok();
}

/// The noun follows the count in the other direction too, so the summary a
/// multi-block pass prints is unchanged.
#[test]
fn test_block_count_agrees() {
    assert_eq!(block_count(0), "0 blocks");
    assert_eq!(block_count(1), "1 block");
    assert_eq!(block_count(7), "7 blocks");
}

/// What the pass kept is said in words the operator can act on, and nothing
/// is said when it kept nothing: a clause of zeros would only add noise.
#[test]
fn test_kept_detail_reads() {
    assert_eq!(kept_detail(&ReclaimReport::default()), "");
    let kept = ReclaimReport {
        leased: 2,
        deferred: 1,
        skipped_refs: 4,
        torn_tail: 3,
        ..ReclaimReport::default()
    };
    assert_eq!(
        kept_detail(&kept),
        " (2 too recent to remove, 1 could not be removed, 4 references not recognised, 3 logs ended mid-record)"
    );
}

/// The apply path says what the repair did only when it did something,
/// and never hides a failure behind a zero count.
#[test]
fn test_repair_summary_reads() {
    assert_eq!(repair_summary(DescriptorRepair::default()), None);
    assert_eq!(
        repair_summary(DescriptorRepair {
            written: 3,
            failed: 0
        })
        .as_deref(),
        Some("Repaired 3 child descriptors.")
    );
    assert_eq!(
        repair_summary(DescriptorRepair {
            written: 3,
            failed: 2
        })
        .as_deref(),
        Some("Repaired 3 child descriptors, 2 failed.")
    );
}

#[test]
fn test_confirm_granted_yes() {
    assert!(confirm_granted("yes"));
    assert!(confirm_granted("y"));
    assert!(confirm_granted("  yes  \n"));
    assert!(confirm_granted("Y"));
    assert!(confirm_granted("YES"));
}

#[test]
fn test_confirm_refuses_non_yes() {
    assert!(!confirm_granted(""));
    assert!(!confirm_granted("no"));
    assert!(!confirm_granted("yes please"));
    assert!(!confirm_granted("n"));
}

/// The summary and the verbose list must read human-readable kind + cause
/// labels, not the Debug enum names. A user scanning either view sees the
/// cause in plain words, not an internal rule id. Pins the regression after
/// the label helpers replaced the former Debug formatting of the variants.
#[test]
fn test_prune_labels_readable() {
    let plan = PrunePlan {
        entries: vec![
            PruneEntry {
                path: "/old/abc".into(),
                kind: PruneKind::Session,
                reason: PruneReason::Ttl,
                last_active: 0,
                action: PruneAction::RemoveDir,
            },
            PruneEntry {
                path: "/idle/def".into(),
                kind: PruneKind::Session,
                reason: PruneReason::EmptyTtl,
                last_active: 0,
                action: PruneAction::RemoveDir,
            },
        ],
        kept: 3,
    };
    for entry in &plan.entries {
        let line = format_prune_entry(entry);
        assert!(
            !line.contains("Ttl") && !line.contains("EmptyTtl") && !line.contains("CapOverflow"),
            "verbose entry must not leak Debug enum names: {line}"
        );
        assert!(
            line.contains(kind_label(entry.kind)) && line.contains(reason_label(entry.reason)),
            "verbose entry must use the shared labels: {line}"
        );
    }
    assert_eq!(kind_label(PruneKind::Session), "session");
    assert_eq!(reason_label(PruneReason::Ttl), "expired");
    assert_eq!(reason_label(PruneReason::EmptyTtl), "idle (empty)");
    assert_eq!(reason_label(PruneReason::CapOverflow), "over retention cap");

    // Summary line: the same labels in the one-screen view format.
    let line = format_summary_count(5, PruneKind::Session, PruneReason::Ttl);
    assert_eq!(line, "  5 session · expired");
    assert!(
        !line.contains("Ttl") && !line.contains("Session"),
        "summary line must not leak Debug enum names: {line}"
    );
    assert_eq!(
        format_summary_count(3, PruneKind::Session, PruneReason::EmptyTtl),
        "  3 session · idle (empty)"
    );
}
