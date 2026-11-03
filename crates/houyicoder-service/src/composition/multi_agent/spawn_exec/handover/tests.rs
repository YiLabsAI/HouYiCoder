//! The wait's decision when a hand-over and the child's own terminal meet.

use std::sync::Arc;
use std::time::Duration;

use houyicoder_api::spawn::{BackgroundLaunch, SpawnFailure, SpawnOutcome};
use houyicoder_async::CancellationToken;
use houyicoder_core::agent::multi_agent::bus_types::ChildRunMode;
use houyicoder_core::agent::multi_agent::ending::EndingClaim;
use houyicoder_protocol::llm::Usage;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::{ParentCancelLink, wait_or_handover};

/// A run that finishes after a reader claimed its ending is reported as the
/// hand-over, not as the answer: the child published a background ending the
/// notification stream carries, so returning the result too would report the
/// same work twice.
#[tokio::test]
async fn test_handover_replaces_answer() {
    let ending = Arc::new(EndingClaim::new(ChildRunMode::Foreground));
    assert!(
        ending.claim_notification(),
        "the reader handed the child over"
    );
    let mut run: JoinHandle<Result<SpawnOutcome, SpawnFailure>> = tokio::spawn(async {
        Ok(SpawnOutcome::foreground(
            "c1",
            "completed",
            "the answer",
            Usage::default(),
        ))
    });
    let outcome = wait_or_handover(
        &mut run,
        &Notify::new(),
        ParentCancelLink::new(None, CancellationToken::new()),
        Arc::clone(&ending),
        None,
        "c1".to_string(),
    )
    .await
    .expect("the wait resolves");
    assert_eq!(
        outcome.background,
        Some(BackgroundLaunch::HandedOver),
        "the queued notification carries the ending, so the turn must not also return it"
    );
    assert!(
        outcome.summary.is_none(),
        "the answer belongs to the notification, not to the turn"
    );
}

/// The same decision holds when the run finishes under a wait budget and the
/// budget's own timer never fires: the claim, not the timer, decides.
#[tokio::test]
async fn test_budget_handover_replaces_answer() {
    let ending = Arc::new(EndingClaim::new(ChildRunMode::Foreground));
    assert!(
        ending.claim_notification(),
        "the reader handed the child over"
    );
    let mut run: JoinHandle<Result<SpawnOutcome, SpawnFailure>> = tokio::spawn(async {
        Ok(SpawnOutcome::foreground(
            "c1",
            "completed",
            "the answer",
            Usage::default(),
        ))
    });
    let outcome = wait_or_handover(
        &mut run,
        &Notify::new(),
        ParentCancelLink::new(None, CancellationToken::new()),
        Arc::clone(&ending),
        Some(Duration::from_secs(30)),
        "c1".to_string(),
    )
    .await
    .expect("the wait resolves");
    assert_eq!(
        outcome.background,
        Some(BackgroundLaunch::HandedOver),
        "the budget did not fire, so only the claim can explain the launch"
    );
}

/// Dropping the wait after a hand-over claimed the ending leaves the child
/// running: the hand-over reported success and the ending arrives as a
/// notification, so a later stop of the turn must not kill the work it freed.
#[test]
fn test_handover_spares_child() {
    let ending = Arc::new(EndingClaim::new(ChildRunMode::Foreground));
    assert!(
        ending.claim_notification(),
        "the reader handed the child over"
    );
    let child = CancellationToken::new();
    let link = ParentCancelLink::new(None, child.clone());
    link.ending().bind(Arc::clone(&ending));
    drop(link);
    assert!(
        !child.is_cancelled(),
        "the handed-over child keeps running on its own token"
    );
}

/// A stop of the turn that arrives after a hand-over leaves the child alone
/// even while the wait is still alive: the forwarding task reads the same
/// claim the hand-over set before it signalled.
#[tokio::test]
async fn test_turn_stop_spares_handover() {
    let ending = Arc::new(EndingClaim::new(ChildRunMode::Foreground));
    assert!(
        ending.claim_notification(),
        "the reader handed the child over"
    );
    let parent = CancellationToken::new();
    let child = CancellationToken::new();
    let mut link = ParentCancelLink::new(Some(parent.clone()), child.clone());
    link.ending().bind(Arc::clone(&ending));
    parent.cancel();
    link.forward_task()
        .expect("the turn's token is linked")
        .await
        .expect("the forwarding task ends when the turn stops");
    assert!(
        !child.is_cancelled(),
        "a stop after the hand-over must not reach the freed child"
    );
}

/// The same stop stops a child nobody handed over, so the escape above is the
/// hand-over's claim and not a forwarding task that never cancels.
#[tokio::test]
async fn test_turn_stop_stops_child() {
    let parent = CancellationToken::new();
    let child = CancellationToken::new();
    let mut link = ParentCancelLink::new(Some(parent.clone()), child.clone());
    parent.cancel();
    link.forward_task()
        .expect("the turn's token is linked")
        .await
        .expect("the forwarding task ends when the turn stops");
    assert!(
        child.is_cancelled(),
        "a turn that stops still stops the child it was waiting on"
    );
}

/// Dropping the wait without a hand-over still stops the child, so the escape
/// above is the claim and not an unused link that never cancels anything.
#[test]
fn test_dropped_link_stops_child() {
    let child = CancellationToken::new();
    drop(ParentCancelLink::new(None, child.clone()));
    assert!(
        child.is_cancelled(),
        "a turn that stops still stops the child it was waiting on"
    );
}
