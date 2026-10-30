//! Classification and countdown mechanics for the auto-approve timer.

use std::time::{Duration, Instant};

use super::Approval;
use houyicoder_protocol::frontend::permission::AskSource;

fn card(validator: &str, source: Option<AskSource>, args: &str) -> Approval {
    Approval {
        tool: "bash".into(),
        args: args.into(),
        reason: "r".into(),
        source,
        validator: Some(validator.into()),
        delegation: None,
        containment_note: None,
        selected: 0,
        expanded: false,
        call_id: "c1".into(),
        options: Vec::new(),
        auto_approve_at: None,
        auto_approve_cancelled: false,
    }
}

/// A fenced compound command and a destructive command are Level 1: the
/// fence backstops the write, so a countdown that lands on Yes-once is safe.
#[test]
fn test_level1_compound_destructive() {
    assert!(card("compound_command", None, "cat > /tmp/x").auto_approve_eligible());
    assert!(card("destructive_command", None, "rm /tmp/x").auto_approve_eligible());
}

/// A protected path, a bounds-widening ask, a network call, and a forced push
/// are Level 2: each needs a human press and never auto-approves.
#[test]
fn test_level2_never_approves() {
    assert!(
        !card(
            "protected_path",
            Some(AskSource::SystemSafety),
            "echo >> settings.json"
        )
        .auto_approve_eligible()
    );
    assert!(!card("path_bounds", None, "cd /etc").auto_approve_eligible());
    assert!(!card("directory_capability", None, "ls /private").auto_approve_eligible());
    assert!(!card("network_egress", None, "curl http://x").auto_approve_eligible());
    assert!(
        !card("git_checkpoint", None, "git push --force origin main").auto_approve_eligible(),
        "a forced push rewrites published history"
    );
}

/// A local git checkpoint is Level 1: the reflog recovers it.
#[test]
fn test_local_checkpoint_level1() {
    assert!(
        card("git_checkpoint", None, "git commit -m x").auto_approve_eligible(),
        "a local commit is recoverable"
    );
    assert!(
        card("git_checkpoint", None, "git branch -d topic").auto_approve_eligible(),
        "a safe branch delete is local"
    );
}

/// A native tool call is Level 1 only when it discards uncommitted work the
/// user flagged; without the flag it stays Level 2.
#[test]
fn test_native_discard_level1() {
    assert!(
        card("tool_native", None, "{\"discard_changes\":true}").auto_approve_eligible(),
        "a discard the user flagged may auto-approve"
    );
    assert!(
        !card("tool_native", None, "{\"discard_changes\":false}").auto_approve_eligible(),
        "a keep does not auto-approve"
    );
}

/// A Level 1 card starts a countdown; a Level 2 card does not.
#[test]
fn test_auto_approve_starts_level1() {
    let mut a = card("compound_command", None, "cat > /tmp/x");
    a.start_auto_approve(7);
    assert!(
        a.auto_approve_at.is_some(),
        "a Level 1 card starts the timer"
    );

    let mut b = card(
        "protected_path",
        Some(AskSource::SystemSafety),
        "echo >> settings.json",
    );
    b.start_auto_approve(7);
    assert!(
        b.auto_approve_at.is_none(),
        "a Level 2 card never starts the timer"
    );
}

/// A zero delay disables the countdown even for a Level 1 card.
#[test]
fn test_zero_delay_disables() {
    let mut a = card("compound_command", None, "cat > /tmp/x");
    a.start_auto_approve(0);
    assert!(a.auto_approve_at.is_none(), "zero delay means no countdown");
}

/// A cursor parked on No by a sticky reject does not start the countdown.
#[test]
fn test_sticky_reject_skips() {
    let mut a = card("compound_command", None, "cat > /tmp/x");
    a.selected = 1;
    a.start_auto_approve(7);
    assert!(
        a.auto_approve_at.is_none(),
        "a No cursor does not count down"
    );
}

/// One navigation key cancels the countdown for the life of the card.
#[test]
fn test_cancel_clears_countdown() {
    let mut a = card("compound_command", None, "cat > /tmp/x");
    a.start_auto_approve(7);
    a.cancel_auto_approve();
    assert!(a.auto_approve_at.is_none(), "cancel clears the deadline");
    assert!(
        a.auto_approve_cancelled,
        "cancel marks the card as user-touched"
    );
}

/// The countdown reports seconds left and fires once the deadline passes.
#[test]
fn test_auto_approve_fires() {
    let mut a = card("compound_command", None, "cat > /tmp/x");
    a.auto_approve_at = Some(Instant::now() + Duration::from_secs(3));
    let now = Instant::now();
    let left = a
        .auto_approve_secs_left(now)
        .expect("a countdown is running");
    assert!(
        (2..=3).contains(&left),
        "about three seconds left, got {left}"
    );
    assert!(!a.auto_approve_expired(now), "not yet expired");
    let later = now + Duration::from_secs(4);
    assert!(a.auto_approve_expired(later), "expired after the deadline");
}
