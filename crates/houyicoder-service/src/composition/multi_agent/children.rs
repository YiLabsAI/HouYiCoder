//! The children a parent is running: who can be steered, and which runs the
//! shutdown has to wait for.
//!
//! A child outlives the call that started it, so the parent keeps two records
//! of it: an entry it can reach by id to steer or stop the child mid-run, and
//! an admission slot the shutdown join waits on until the child's run is over.

use super::*;

/// A running child the parent can reach by id: the runner to steer or stop,
/// the token that ends it, and the two channels that let the reader take it
/// over or learn how it ended.
pub(super) struct ChildEntry {
    pub(super) runner: Weak<Runner>,
    pub(super) cancel: CancellationToken,
    /// The mode this child's ending is published with; a foreground child
    /// carries a flag the runtime can flip to hand it to the background.
    pub(super) backgrounded: Option<Arc<AtomicBool>>,
    /// Raised when the reader takes a running child over.
    pub(super) handover: Option<Arc<Notify>>,
}

/// How the shutdown join observes one child.
///
/// A child started in the background has no caller left to await it, so the
/// join owns its task handle. A child started in the foreground still has a
/// caller awaiting it, so the join holds an abort handle and waits on the
/// completion signal instead.
pub(super) enum ChildWatch {
    Detached(JoinHandle<()>),
    Awaited { abort: AbortHandle },
}

/// One admitted child's shutdown credential: the lifecycle token that ends it
/// (the exact token carried by the child handle) and the completion signal the
/// join waits on. The watch arrives later, once the task exists; before that
/// the token and the signal already cover the child, so a shutdown landing in
/// between still ends it and still waits for it.
pub(super) struct ChildCredential {
    pub(super) close: CancellationToken,
    pub(super) finished: Receiver<()>,
    pub(super) watch: Option<ChildWatch>,
}

/// What admitting a child hands back: the slot the registry filed it under,
/// the token to pass to the spawn as the child's cancel, and the completion
/// sender to move into the task.
pub(super) struct ChildAdmission {
    pub(super) slot: u64,
    pub(super) close: CancellationToken,
    pub(super) finished: Sender<()>,
}

/// The children currently running, with the gate that refuses new ones once a
/// shutdown starts. A child is admitted before its first side effect and
/// watched once its task exists; both steps refuse after the gate closes.
#[derive(Default)]
pub(super) struct RunningChildren {
    pub(super) closing: bool,
    pub(super) next_slot: u64,
    pub(super) children: HashMap<u64, ChildCredential>,
    /// The children the close step set aside, waiting for the join to take.
    pub(super) drained: Vec<(u64, ChildCredential)>,
}

impl RunningChildren {
    /// Admit a child before its first side effect.
    ///
    /// The completion channel is created here, so the token and the signal are
    /// both live before the child exists. None once the gate is closed: a
    /// shutdown in progress refuses new children at the door.
    pub(super) fn admit(&mut self, close: CancellationToken) -> Option<ChildAdmission> {
        if self.closing {
            return None;
        }
        let slot = self.next_slot;
        self.next_slot += 1;
        let (finished, receiver) = channel::<()>();
        self.children.insert(
            slot,
            ChildCredential {
                close: close.clone(),
                finished: receiver,
                watch: None,
            },
        );
        Some(ChildAdmission {
            slot,
            close,
            finished,
        })
    }

    /// Record how the shutdown join observes an admitted child. Err hands the
    /// watch back once the slot is gone, which means a shutdown set it aside:
    /// the caller must then wait on the task itself.
    pub(super) fn watch(&mut self, slot: u64, watch: ChildWatch) -> Result<(), ChildWatch> {
        match self.children.get_mut(&slot) {
            Some(child) => {
                child.watch = Some(watch);
                Ok(())
            }
            None => Err(watch),
        }
    }

    /// The child's run is over; drop its slot so a later join waits only for
    /// children still running.
    pub(super) fn release(&mut self, slot: u64) {
        self.children.remove(&slot);
    }

    /// Close the gate and set the running children aside for the join. The
    /// caller holds this lock across the cancel pass, which is what makes a
    /// child admitted against a shutdown see either a cancelled token or a
    /// closed gate, never neither. Setting aside happens only while the drained
    /// list is empty, so a repeated close cannot discard children the join has
    /// not taken yet.
    pub(super) fn close_and_drain(&mut self) {
        self.closing = true;
        if self.drained.is_empty() {
            self.drained = take(&mut self.children).into_iter().collect();
        }
    }

    /// Take the children the close step set aside. Empty when the gate never
    /// closed, or after a prior take.
    pub(super) fn take_drained(&mut self) -> Vec<(u64, ChildCredential)> {
        take(&mut self.drained).into_iter().collect()
    }
}

/// A claim on a child session id a spawn is about to continue.
///
/// The registry holds a child only once its runner exists, so two resumes of
/// one session can both pass the liveness check before either registers. The
/// claim closes that window: the second resume is refused instead of driving
/// the same session twice. It drops when the task ends, however it ends, so a
/// refused or panicked construction releases the id rather than stranding it.
pub(in crate::composition) struct ChildReservation {
    id: String,
    reserved: Arc<Mutex<HashSet<String>>>,
}

impl ChildReservation {
    pub(super) fn new(id: String, reserved: Arc<Mutex<HashSet<String>>>) -> Self {
        Self { id, reserved }
    }
}

impl Drop for ChildReservation {
    fn drop(&mut self) {
        if let Ok(mut g) = self.reserved.lock() {
            g.remove(&self.id);
        }
    }
}

/// Releases a child's slot when its task ends, however the task ends.
///
/// A task ends by reaching the child's terminal, by refusing to construct one,
/// or by being aborted. Each must release the slot, because the slot is what a
/// later shutdown join waits on: one left behind makes the join wait out a
/// child that is no longer there, and a long-lived session collects one per
/// delegation.
pub(super) struct AdmissionLease {
    running: Arc<Mutex<RunningChildren>>,
    slot: u64,
}

impl AdmissionLease {
    pub(super) fn new(running: Arc<Mutex<RunningChildren>>, slot: u64) -> Self {
        Self { running, slot }
    }
}

impl Drop for AdmissionLease {
    fn drop(&mut self) {
        if let Ok(mut g) = self.running.lock() {
            g.release(self.slot);
        }
    }
}

/// Wait out one child the shutdown set aside, under a bounded budget.
///
/// The budget is per child, generous for a real finalize tail, with an abort
/// backstop so a wedged task cannot hold the shutdown lock forever. A detached
/// child keeps the full result match, so a normal end, a panic, an abort, and a
/// timeout stay four distinct outcomes and the panic is announced. An awaited
/// child confirms its completion signal closed rather than treating the abort
/// as the stop. A child whose task was never watched waits on that signal alone.
pub(super) async fn join_child(child: ChildCredential, budget: Duration) {
    match child.watch {
        Some(ChildWatch::Detached(mut task)) => {
            let stopped = match timeout(budget, &mut task).await {
                Ok(outcome) => outcome,
                Err(_) => {
                    task.abort();
                    task.await
                }
            };
            if let Err(err) = stopped
                && err.is_panic()
            {
                tracing::warn!("a child task panicked during shutdown: {err}");
            }
        }
        Some(ChildWatch::Awaited { abort }) => {
            let mut finished = child.finished;
            if timeout(budget, &mut finished).await.is_err() {
                abort.abort();
                let _ = finished.await;
            }
        }
        None => {
            let _ = child.finished.await;
        }
    }
}
