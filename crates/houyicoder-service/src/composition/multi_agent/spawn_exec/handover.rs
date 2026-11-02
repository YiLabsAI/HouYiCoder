//! The foreground wait: who owns the child's lifecycle token while the turn
//! waits, and what ends the wait.
//!
//! A foreground delegation holds the parent's turn open until the child ends.
//! Two things can cut that wait short: the turn itself being stopped, and the
//! reader handing the child to the background. The child's own ceiling can also
//! end the child while the turn keeps running, so the two lifecycles are
//! related but not the same one.

use houyicoder_api::spawn::SpawnOutcome;
use houyicoder_async::CancellationToken;

use super::SpawnFailure;
use std::panic::resume_unwind;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// Carries the turn's stop to the child's own lifecycle token, one way.
///
/// The child must not hold the turn's token itself. One token shared by both
/// cancels in both directions: a child that reaches its own ceiling would stop
/// the turn that started it, and a turn the user stops would reach a child the
/// reader has already handed to the background. The link therefore forwards a
/// single direction, and only while the turn is still waiting on the child.
///
/// Dropping the link stops the child. The turn stops waiting in three ways —
/// it finished, its future was dropped, or it handed the child to the
/// background — and only the last one leaves the child running, so it is the
/// only one that releases the link first.
pub(super) struct ParentCancelLink {
    child: CancellationToken,
    forward: Option<JoinHandle<()>>,
    forwarding: bool,
}

impl ParentCancelLink {
    /// Bind a child token to the turn that started it.
    pub(super) fn new(parent: Option<CancellationToken>, child: CancellationToken) -> Self {
        let forward = parent.map(|parent| {
            let child = child.clone();
            tokio::spawn(async move {
                parent.cancelled().await;
                child.cancel();
            })
        });
        Self {
            child,
            forward,
            forwarding: true,
        }
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
        if self.forwarding {
            self.child.cancel();
        }
    }
}

/// Wait for the child's outcome, or return the launch as soon as the reader
/// hands the child to the background.
///
/// The child's own outcome is checked first: a hand-over that lands in the same
/// instant the child finishes must not report a launch for work that is already
/// done, because the parent would then never see the answer it already paid
/// for. Once handed over, the task keeps running on its own and the child's
/// ending reaches the parent as a notification, which is what the flag the
/// runtime flipped is for.
pub(super) async fn await_child_or_handover(
    run: &mut JoinHandle<Result<SpawnOutcome, SpawnFailure>>,
    handover: &Notify,
    link: ParentCancelLink,
    child_str: String,
) -> Result<SpawnOutcome, SpawnFailure> {
    tokio::select! {
        biased;
        outcome = &mut *run => match outcome {
            Ok(result) => result,
            // A panic inside the task would otherwise reach the model as an
            // ordinary refusal, so report it as the panic it is.
            Err(join) => resume_unwind(join.into_panic()),
        },
        _ = handover.notified() => {
            // The reader took the child over, so it now runs on its own token
            // and the turn's later stop leaves it alone.
            link.release();
            Ok(SpawnOutcome::background_started(child_str))
        }
    }
}

/// Wait for a child run that never reported a child, reporting its own result.
pub(super) async fn await_child_end(
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
