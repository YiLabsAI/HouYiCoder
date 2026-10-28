//! Tests for the worktree mapping.

use std::path::PathBuf;

use houyicoder_core::agent::worktree::changes::WorktreeChanges;
use houyicoder_core::agent::worktree::identify::WorktreeIdentity;
use houyicoder_core::agent::worktree::preview::MergeState;
use houyicoder_core::agent::worktree::preview::WorktreePreview;
use houyicoder_core::agent::worktree::session::WorktreeError;
use houyicoder_protocol::frontend::worktree::WorktreeRemoveOutcome;

use super::map_entered;
use super::map_preview;
use super::map_removal;

fn sample_preview(merge: MergeState) -> WorktreePreview {
    WorktreePreview {
        identity: WorktreeIdentity {
            path: PathBuf::from("/repo/.worktrees/bold-otter"),
            common_dir: PathBuf::from("/repo/.git"),
            branch: "worktree/bold-otter".to_string(),
            head: "abc123".to_string(),
            locked: true,
        },
        changes: WorktreeChanges {
            tracked_dirty: 2,
            untracked: 5,
            ignored_build_dirs: vec!["target".to_string(), "node_modules".to_string()],
            ignored_unknown: 1,
        },
        merge,
        unmerged_commits: Some(3),
        leased: true,
        locked: false,
        token: "tok-9".to_string(),
    }
}

/// Every measured fact reaches the view under its own name, and the lock the
/// view reports is the one the preview measured rather than the identity's.
#[test]
fn test_preview_maps_every_measure() {
    let view = map_preview(
        "bold-otter",
        sample_preview(MergeState::Merged {
            base: "main".to_string(),
        }),
    );
    assert_eq!(view.slug, "bold-otter");
    assert_eq!(view.path, "/repo/.worktrees/bold-otter");
    assert_eq!(view.branch, "worktree/bold-otter");
    assert_eq!(view.head, "abc123");
    assert_eq!(view.tracked_dirty, 2);
    assert_eq!(view.untracked, 5);
    assert_eq!(view.ignored_build_dirs, ["target", "node_modules"]);
    assert_eq!(view.ignored_unknown, 1);
    assert!(view.merged);
    assert_eq!(view.base_branch.as_deref(), Some("main"));
    assert_eq!(view.unmerged_commits, Some(3));
    assert!(view.leased);
    assert!(
        !view.locked,
        "the preview's lock answer, not the identity's"
    );
    assert_eq!(view.token, "tok-9");
}

/// A branch the base does not contain stays distinct from a merge the host
/// could not judge, and an uncounted commit total stays absent rather than
/// becoming zero.
#[test]
fn test_unknown_merge_stays_absent() {
    let not_merged = map_preview(
        "bold-otter",
        sample_preview(MergeState::NotMerged {
            base: "trunk".to_string(),
        }),
    );
    assert!(!not_merged.merged);
    assert_eq!(not_merged.base_branch.as_deref(), Some("trunk"));

    let mut unknown = sample_preview(MergeState::Unknown { base: None });
    unknown.unmerged_commits = None;
    let view = map_preview("bold-otter", unknown);
    assert!(!view.merged);
    assert_eq!(view.base_branch, None);
    assert_eq!(view.unmerged_commits, None);

    let mut unanswered = sample_preview(MergeState::Unknown {
        base: Some("gone".to_string()),
    });
    unanswered.unmerged_commits = None;
    let view = map_preview("bold-otter", unanswered);
    assert!(!view.merged);
    assert_eq!(
        view.base_branch.as_deref(),
        Some("gone"),
        "a base Git never answered about is still the base that was asked"
    );
    assert_eq!(view.unmerged_commits, None);
}

/// Each unfinished removal keeps what it names, so the pane reports what
/// survived instead of a clean result.
#[test]
fn test_removal_maps_unfinished_outcomes() {
    let stale = map_removal(Err(WorktreeError::Stale {
        slug: "bold-otter".to_string(),
    }));
    assert_eq!(stale.unwrap(), WorktreeRemoveOutcome::Stale);

    let incomplete = map_removal(Err(WorktreeError::RemovalIncomplete {
        path: PathBuf::from("/repo/.worktrees/bold-otter"),
        branch: "worktree/bold-otter".to_string(),
        reason: "branch is checked out elsewhere".to_string(),
    }));
    match incomplete.unwrap() {
        WorktreeRemoveOutcome::Incomplete {
            path,
            branch,
            reason,
        } => {
            assert_eq!(path, "/repo/.worktrees/bold-otter");
            assert_eq!(branch, "worktree/bold-otter");
            assert_eq!(reason, "branch is checked out elsewhere");
        }
        other => panic!("expected an incomplete removal, got {other:?}"),
    }

    let pending = map_removal(Err(WorktreeError::CleanupPending {
        path: PathBuf::from("/repo/.worktrees/bold-otter"),
        reason: "directory survived".to_string(),
    }));
    match pending.unwrap() {
        WorktreeRemoveOutcome::Pending { path, reason } => {
            assert_eq!(path, "/repo/.worktrees/bold-otter");
            assert_eq!(reason, "directory survived");
        }
        other => panic!("expected a pending removal, got {other:?}"),
    }
}

/// A refusal that changed nothing returns to the caller to report, and a
/// removal that ran reports that it finished.
#[test]
fn test_removal_passes_refusal_through() {
    assert_eq!(map_removal(Ok(())).unwrap(), WorktreeRemoveOutcome::Removed);
    let refusal = map_removal(Err(WorktreeError::NotARepo));
    assert!(matches!(refusal, Err(WorktreeError::NotARepo)));
}

/// Entering reports the directory and branch the host moved to.
#[test]
fn test_entered_reports_location() {
    let view = map_entered(
        "/repo/.worktrees/bold-otter".to_string(),
        "worktree/bold-otter".to_string(),
    );
    assert_eq!(view.path, "/repo/.worktrees/bold-otter");
    assert_eq!(view.branch, "worktree/bold-otter");
}
