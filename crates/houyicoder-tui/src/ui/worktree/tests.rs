//! Unit tests for the removal draft: what it reuses, what it refuses, and
//! what it submits.

use houyicoder_protocol::envelope::RequestId;

use crate::test_harness::{worktree_path, worktree_preview};
use crate::ui::{BeginOutcome, RemovalDraft, RemoveFocus, WorktreeLevel};

#[test]
fn test_draft_reuses_preview() {
    let mut draft = RemovalDraft::default();
    assert_eq!(
        draft.begin(&worktree_path("alpha"), WorktreeLevel::List),
        BeginOutcome::Reading,
        "a first visit has nothing to show"
    );
    draft.accept(worktree_preview("alpha"));
    assert_eq!(
        draft.begin(&worktree_path("alpha"), WorktreeLevel::Detail),
        BeginOutcome::Ready,
        "the same worktree keeps the preview it already has"
    );
    assert_eq!(
        draft.begin(&worktree_path("beta"), WorktreeLevel::Detail),
        BeginOutcome::Reading,
        "another worktree needs its own read"
    );
    assert!(
        draft.preview().is_none(),
        "moving to another worktree drops the old preview"
    );
}

#[test]
fn test_draft_reuses_pending_read() {
    let mut draft = RemovalDraft::default();
    draft.begin(&worktree_path("alpha"), WorktreeLevel::List);
    draft.expect_preview(RequestId(3));
    assert_eq!(
        draft.begin(&worktree_path("alpha"), WorktreeLevel::Confirm),
        BeginOutcome::Reading
    );
    assert_eq!(
        draft.preview_request(),
        Some(RequestId(3)),
        "the read already on its way is the one the draft waits for"
    );
}

#[test]
fn test_draft_refuses_while_removing() {
    let mut draft = RemovalDraft::default();
    draft.begin(&worktree_path("alpha"), WorktreeLevel::List);
    draft.accept(worktree_preview("alpha"));
    draft
        .take_submission(RequestId(1))
        .expect("a free worktree submits");
    assert_eq!(
        draft.begin(&worktree_path("beta"), WorktreeLevel::List),
        BeginOutcome::Busy,
        "a removal with the host is not displaced by pointing elsewhere"
    );
    assert_eq!(
        draft.path(),
        Some(worktree_path("alpha").as_str()),
        "the draft stays on the worktree whose answer is coming"
    );
    draft.finish_removal();
    assert_eq!(
        draft.begin(&worktree_path("beta"), WorktreeLevel::List),
        BeginOutcome::Reading
    );
}

#[test]
fn test_order_refused_while_leased() {
    let mut draft = RemovalDraft::default();
    draft.begin(&worktree_path("alpha"), WorktreeLevel::List);
    let mut preview = worktree_preview("alpha");
    preview.leased = true;
    draft.accept(preview);
    assert!(
        draft.take_submission(RequestId(1)).is_none(),
        "a worktree a session holds is not submitted"
    );
    assert!(!draft.removing(), "a refused order leaves nothing running");
}

#[test]
fn test_order_sent_once() {
    let mut draft = RemovalDraft::default();
    draft.begin(&worktree_path("alpha"), WorktreeLevel::List);
    draft.accept(worktree_preview("alpha"));
    let order = draft
        .take_submission(RequestId(7))
        .expect("a free worktree submits");
    assert_eq!(order.path, worktree_path("alpha"));
    assert_eq!(order.token, "digest-1");
    assert!(!order.delete_unmerged_branch);
    assert!(
        draft.take_submission(RequestId(8)).is_none(),
        "a second Enter does not submit the same removal again"
    );
    draft.finish_removal();
    assert!(
        draft.take_submission(RequestId(9)).is_some(),
        "the answer reopens the retry"
    );
}

#[test]
fn test_branch_starts_kept() {
    let mut draft = RemovalDraft::default();
    assert!(!draft.delete_unmerged_branch());
    draft.toggle_branch_deletion();
    assert!(draft.delete_unmerged_branch());
    draft.clear();
    assert!(
        !draft.delete_unmerged_branch(),
        "a new draft starts by keeping the branch"
    );
    assert_eq!(draft.focus(), RemoveFocus::Cancel);
}
