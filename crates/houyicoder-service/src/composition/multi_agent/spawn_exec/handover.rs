//! The foreground wait: who owns the child's lifecycle token while the turn
//! waits, and what ends the wait.
//!
//! A foreground delegation holds the parent's turn open until the child ends.
//! Three things can end that wait early: the turn itself being stopped, the
//! reader handing the child to the background, and the wait budget elapsing.
//! The child's own ceiling can also end the child while the turn keeps
//! running, so the two lifecycles are related but not the same one.

use houyicoder_api::spawn::SpawnOutcome;
use houyicoder_async::CancellationToken;
use houyicoder_core::agent::multi_agent::ending::EndingClaim;

use super::SpawnFailure;
use std::panic::resume_unwind;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::{JoinError, JoinHandle};
use tokio::time::sleep;

/// The ending claim a foreground wait reads, filled in by the child's own task.
///
/// The wait exists before the child does, because a turn stopped while the
/// child starts must still stop it. The claim only exists once the child is
/// built, and the reader can hand the child over the moment it is announced,
/// so the child's task binds the claim before announcing. That order is what
/// makes a stop of the turn after a hand-over leave the child running: the
/// link's forwarding task and its drop guard both read the claim here.
#[derive(Clone, Default)]
pub(super) struct EndingBinding(Arc<OnceLock<Arc<EndingClaim>>>);

impl EndingBinding {
    /// Share the claim the child publishes its ending through.
    pub(super) fn bind(&self, ending: Arc<EndingClaim>) {
        if self.0.set(ending).is_err() {
            tracing::warn!("a child's ending claim was bound to its cancel link twice");
        }
    }

    /// Whether the reader already claimed this child's ending for the
    /// notification stream, which is what a hand-over does before it signals.
    fn notifies_parent(&self) -> bool {
        self.0.get().is_some_and(|ending| ending.notifies_parent())
    }
}

/// Carries the turn's stop to the child's own lifecycle token, one way.
///
/// The child must not hold the turn's token itself. One token shared by both
/// cancels in both directions: a child that reaches its own ceiling would stop
/// the turn that started it, and a turn the user stops would reach a child the
/// reader has already handed to the background. The link therefore forwards a
/// single direction, and only while the turn is still waiting on the child.
///
/// Dropping the link stops the child. The turn stops waiting in four ways —
/// it finished, its future was dropped, it handed the child to the
/// background, or its wait budget elapsed — and the last two leave the child
/// running, so they are the only ones that release the link first. A drop
/// that follows a hand-over must also leave the child alone: the claim is
/// what reports the hand-over, and it lands before the signal, so a turn
/// stopped in between still reads it here.
pub(super) struct ParentCancelLink {
    child: CancellationToken,
    forward: Option<JoinHandle<()>>,
    forwarding: bool,
    ending: EndingBinding,
}

impl ParentCancelLink {
    /// Bind a child token to the turn that started it.
    ///
    /// The child reports its ending claim only once it exists, and the link
    /// must already be able to stop it before that, so the child's task binds
    /// the claim through the binding this exposes.
    pub(super) fn new(parent: Option<CancellationToken>, child: CancellationToken) -> Self {
        let ending = EndingBinding::default();
        let forward = parent.map(|parent| {
            let child = child.clone();
            let ending = ending.clone();
            tokio::spawn(async move {
                parent.cancelled().await;
                if !ending.notifies_parent() {
                    child.cancel();
                }
            })
        });
        Self {
            child,
            forward,
            forwarding: true,
            ending,
        }
    }

    /// The slot the child's own task fills in before it announces the child.
    pub(super) fn ending(&self) -> EndingBinding {
        self.ending.clone()
    }

    /// The task that applies a stop of the turn to the child, so a test can
    /// wait for that decision instead of racing it.
    #[cfg(test)]
    pub(super) fn forward_task(&mut self) -> Option<&mut JoinHandle<()>> {
        self.forward.as_mut()
    }

    /// Let go: the child now runs on its own token, and a later stop of the
    /// turn does not reach it. Called once the reader hands the child to the
    /// background, which is the whole point of that hand-over.
    pub(super) fn release(mut self) {
        self.disarm();
        self.forwarding = false;
    }

    fn disarm(&mut self) {
        if let Some(task) = self.forward.take() {
            task.abort();
        }
    }
}

impl Drop for ParentCancelLink {
    fn drop(&mut self) {
        self.disarm();
        if self.forwarding && !self.ending.notifies_parent() {
            self.child.cancel();
        }
    }
}

/// Report a finished run, unless a reader already claimed its ending.
///
/// A reader can hand the child over while its run is finishing. The hand-over
/// claims the ending for the notification stream before it signals, so the
/// child publishes a background ending; returning the inline result as well
/// would report the same work twice. The launch replaces the answer because
/// the answer is not lost: the notification the publisher queued carries it.
fn settled_outcome(
    joined: Result<Result<SpawnOutcome, SpawnFailure>, JoinError>,
    link: ParentCancelLink,
    ending: &EndingClaim,
    child_str: String,
) -> Result<SpawnOutcome, SpawnFailure> {
    let outcome = match joined {
        Ok(result) => result,
        // A panic inside the task would otherwise reach the model as an
        // ordinary refusal, so report it as the panic it is.
        Err(join) => resume_unwind(join.into_panic()),
    };
    if ending.notifies_parent() {
        link.release();
        return Ok(SpawnOutcome::background_handed_over(child_str));
    }
    outcome
}

/// Wait for the child's outcome, return the launch when the reader hands the
/// child to the background, or send the child to the background once its
/// foreground wait budget elapses.
///
/// The wait and the child's status publisher race for one ending claim, and
/// whichever claims it first decides how the parent learns the result. A
/// hand-over or an elapsed budget claims it for the notification stream; a run
/// that reaches a terminal first claims it for the caller. A wait that loses
/// that race returns the run's own outcome instead of a launch, because the
/// launch text promises a notification nobody would send. Once in the
/// background the task keeps running on its own token.
pub(super) async fn wait_or_handover(
    run: &mut JoinHandle<Result<SpawnOutcome, SpawnFailure>>,
    handover: &Notify,
    link: ParentCancelLink,
    ending: Arc<EndingClaim>,
    wait_budget: Option<Duration>,
    child_str: String,
) -> Result<SpawnOutcome, SpawnFailure> {
    let Some(budget) = wait_budget else {
        return tokio::select! {
            biased;
            outcome = &mut *run => settled_outcome(outcome, link, &ending, child_str),
            _ = handover.notified() => {
                // The reader took the child over, so it now runs on its own
                // token and the turn's later stop leaves it alone.
                link.release();
                Ok(SpawnOutcome::background_handed_over(child_str))
            }
        };
    };
    // A sub-second budget still reads as a wait the child exceeded, so the
    // model-facing sentence never says it held the turn for zero seconds.
    let wait_secs = (budget.as_millis().div_ceil(1000) as u64).max(1);
    tokio::select! {
        biased;
        outcome = &mut *run => settled_outcome(outcome, link, &ending, child_str),
        _ = handover.notified() => {
            // The hand-over claimed the ending before raising this signal, so
            // the child keeps running and publishes a background ending.
            link.release();
            Ok(SpawnOutcome::background_handed_over(child_str))
        }
        _ = sleep(budget) => match resolve_elapsed_budget(&ending) {
            // The wait gave up: the child keeps running on its own token and
            // its ending reaches the parent as a notification.
            ElapsedBudget::Release => {
                link.release();
                Ok(SpawnOutcome::background_after_wait(child_str, wait_secs))
            }
            // A hand-over claimed the ending between its claim and its signal,
            // so the turn is released the same way that signal would have
            // released it. Awaiting the run here would block a turn the reader
            // already freed and report the same answer twice.
            ElapsedBudget::HandedOver => {
                link.release();
                Ok(SpawnOutcome::background_handed_over(child_str))
            }
            // The run claimed the ending while it was still finalizing, so its
            // result is about to land. A launch here would drop an answer the
            // parent already paid for and promise a notification never sent.
            ElapsedBudget::WaitForRun => match (&mut *run).await {
                Ok(result) => result,
                Err(join) => resume_unwind(join.into_panic()),
            },
        },
    }
}

/// Who decided a foreground wait's ending by the time its budget elapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElapsedBudget {
    /// No one had claimed it: the budget claims it for the notification stream.
    Release,
    /// The reader's hand-over claimed it first, so the wait reports the same
    /// launch the hand-over signal would have produced.
    HandedOver,
    /// The run's terminal claimed the caller, so its result is on the way.
    WaitForRun,
}

/// Read the ending's state for an elapsed budget, claiming it when no one has.
///
/// The claim is one atomic transition, so a loss does not say which side won.
/// A hand-over claims for the notification stream and only then raises its
/// signal, so this can observe a loss to a hand-over that has not signalled
/// yet; treating that loss as the run's means the turn never gets released.
pub(crate) fn resolve_elapsed_budget(ending: &EndingClaim) -> ElapsedBudget {
    if ending.claim_notification() {
        ElapsedBudget::Release
    } else if ending.notifies_parent() {
        ElapsedBudget::HandedOver
    } else {
        ElapsedBudget::WaitForRun
    }
}

/// Wait for a child run that never reported a child, reporting its own result.
pub(super) async fn wait_for_child(
    run: JoinHandle<Result<SpawnOutcome, SpawnFailure>>,
    link: ParentCancelLink,
) -> Result<SpawnOutcome, SpawnFailure> {
    let result = match run.await {
        Ok(result) => result,
        Err(join) => resume_unwind(join.into_panic()),
    };
    drop(link);
    result
}

#[cfg(test)]
mod tests;
