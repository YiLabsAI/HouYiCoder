use super::*;
use houyicoder_service::session_prune::{PruneAction, PruneEntry, PruneKind, PruneReason};

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
