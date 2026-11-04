//! One child's execution, from construction to its finalize tail.
//!
//! A child outlives the call that started it. A background delegation returns
//! as soon as the child exists; a foreground one returns as soon as the child
//! ends or the reader takes it over. Both shapes therefore run the child on a
//! task of its own, and that task holds the concurrency slot, the admission
//! slot, and the completion signal until the child is finished.

use std::time::Duration;

use super::super::{AdmissionLease, ChildHandle};
use super::ceiling::{DEFAULT_IDLE_TIMEOUT, DEFAULT_MAX_WALL_CLOCK, watch_liveness};
use super::handover::EndingBinding;
use super::*;

/// Everything one child's run is built from.
///
/// The two shapes differ only in who waits: a background child is detached, a
/// foreground child is supervised. Keeping the shared parts in one value is
/// what stops the two from drifting apart on what a child is given.
pub(super) struct ChildExecution {
    pub(super) runtime: MultiAgentRuntime,
    pub(super) request: SpawnRequest,
    pub(super) hooks: Option<Arc<dyn HookFire>>,
    pub(super) parent: SessionId,
    pub(super) completion: Sender<()>,
    pub(super) running: Arc<Mutex<RunningChildren>>,
    pub(super) admission: u64,
    /// The claim on a resumed session id, held for the life of the task so the
    /// id cannot be driven twice.
    pub(super) reservation: Option<ChildReservation>,
    /// The slot a supervised child fills in before announcing itself, so a
    /// stop of the turn after a hand-over reads the claim rather than
    /// cancelling the child the reader just freed. A detached child has no
    /// waiting turn to release, so it has no slot.
    pub(super) ending_binding: Option<EndingBinding>,
}

impl ChildExecution {
    /// Start the child and report its id the moment it exists, leaving it to
    /// run on its own.
    ///
    /// The permit, the completion signal, and the admission slot ride the task,
    /// so a caller dropped at any await drops none of them. The child's ending
    /// reaches the parent through the bus rather than through a return value,
    /// because no caller is left to receive one.
    pub(super) fn detach(
        self,
        permit: OwnedSemaphorePermit,
        started: Sender<Result<String, SpawnError>>,
    ) -> JoinHandle<()> {
        let ChildExecution {
            runtime,
            request,
            hooks,
            parent,
            completion,
            running,
            admission,
            reservation,
            ending_binding: _,
        } = self;
        let subagent_type = request.subagent_type.clone();
        let description = request.prompt_summary.clone();
        let task = request.prompt.clone();
        // Two different bounds: the idle window cuts a run that stopped
        // producing, the wall clock caps one that never stops. A caller's
        // timeout_secs is the second, not the first.
        let idle_timeout = DEFAULT_IDLE_TIMEOUT;
        let max_wall_clock = request
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_MAX_WALL_CLOCK);
        tokio::spawn(async move {
            // The completion signal fires when this task ends, however it ends:
            // the guard rides the whole task, so the shutdown join's wait on it
            // observes the finalize tail too. The permit releases at the same
            // point, so the slot frees when the child completes.
            let _completion = completion;
            let _permit = permit;
            // The resumed session stays claimed until this task ends, however
            // it ends: a second spawn of the same session is refused for as
            // long as the first is still driving it.
            let _reservation = reservation;
            // The admission leaves with the task, so a refused construction
            // releases its slot instead of leaving one a later join waits on.
            let _lease = AdmissionLease::new(running, admission);
            // Construction is the first side effect; a refusal here is reported
            // through the started channel so the caller states the honest
            // reason rather than inventing a child.
            let handle = match spawn_child(request).await {
                Ok(h) => h,
                Err(e) => {
                    drop(started.send(Err(e)));
                    return;
                }
            };
            let child_sid = handle.session;
            let child_str = child_sid.to_string();
            record_provenance(
                &runtime.descriptor_store,
                child_sid,
                parent,
                &subagent_type,
                &child_str,
            );
            let child = ChildDescriptor::new(
                child_str.clone(),
                subagent_type.clone(),
                ChildRunMode::Background,
            )
            .with_description(description);
            announce(&runtime, &handle, &child_str, child.clone(), None);
            // Construction succeeded; report the child id before the slower
            // Start hook and run so the caller returns promptly after the child
            // is real.
            drop(started.send(Ok(child_str.clone())));
            // The Start hook dispatch can wait on a command hook. It runs in the
            // task, not in the caller, because a caller dropped at that wait
            // (Esc mid-turn) would leave the child announced and registered
            // with nothing driving it to a terminal.
            fire_subagent_start(hooks.as_ref(), parent, &child_str, &subagent_type).await;
            // A background delegation carries the same ceiling a foreground one
            // does: the model asks for a bound on the work, not for a bound that
            // only applies while it happens to be waiting. The child publishes
            // its own ending, so the ceiling only has to stop it, and that
            // ending reports the interrupt the stop produces rather than the
            // deadline behind it. Only a caller that waits can be told which of
            // the two ended the child.
            let ceiling = handle.cancel.clone();
            let cut_reason = Arc::clone(&handle.cut_reason);
            let liveness = Arc::clone(&handle.liveness);
            let run = finalize_child(&runtime, handle, parent, child_sid, child, hooks, task);
            watch_liveness(
                run,
                liveness,
                idle_timeout,
                max_wall_clock,
                ceiling,
                cut_reason,
            )
            .await;
        })
    }

    /// Start the child and report its ending to the caller that waits on it.
    ///
    /// The caller may hand the child to the background while it waits, so the
    /// task also carries the channel that hands it over. The completion signal
    /// and the admission slot ride the task the same way they do when the child
    /// is detached.
    pub(super) fn supervise(
        self,
        slot: OwnedSemaphorePermit,
        handover: Arc<Notify>,
        started: Sender<(String, Arc<EndingClaim>)>,
    ) -> JoinHandle<Result<SpawnOutcome, SpawnFailure>> {
        let ChildExecution {
            runtime,
            request,
            hooks,
            parent,
            completion,
            running,
            admission,
            reservation,
            ending_binding,
        } = self;
        let subagent_type = request.subagent_type.clone();
        let description = request.prompt_summary.clone();
        let task = request.prompt.clone();
        // Two different bounds: the idle window cuts a run that stopped
        // producing, the wall clock caps one that never stops. A caller's
        // timeout_secs is the second, not the first.
        let idle_timeout = DEFAULT_IDLE_TIMEOUT;
        let max_wall_clock = request
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_MAX_WALL_CLOCK);
        tokio::spawn(async move {
            // The completion signal fires when this task ends, however it ends:
            // the guard rides the whole task, so the shutdown join's wait on it
            // observes the finalize tail too.
            let _completion = completion;
            let _slot = slot;
            // The resumed session stays claimed until this task ends, however
            // it ends: a second spawn of the same session is refused for as
            // long as the first is still driving it. The claim also survives a
            // hand-over, because the child keeps running either way.
            let _reservation = reservation;
            // The admission leaves with the task, so a refused construction
            // releases its slot instead of leaving one a later join waits on.
            let _lease = AdmissionLease::new(running, admission);
            let handle = spawn_child(request).await.map_err(map_spawn_err)?;
            let child_sid = handle.session;
            let child_str = child_sid.to_string();
            record_provenance(
                &runtime.descriptor_store,
                child_sid,
                parent,
                &subagent_type,
                &child_str,
            );
            let child = ChildDescriptor::new(
                child_str.clone(),
                subagent_type.clone(),
                ChildRunMode::Foreground,
            )
            .with_description(description);
            // The claim is bound before the child is announced: the reader can
            // hand it over from that moment, and a turn stopped after the
            // hand-over must read the claim here rather than cancel the child
            // the reader just freed.
            if let Some(binding) = ending_binding {
                binding.bind(Arc::clone(&handle.ending));
            }
            // The hand-over channel is registered beside the live runner, so the
            // reader can take this child over while the caller waits on it.
            announce(
                &runtime,
                &handle,
                &child_str,
                child.clone(),
                Some(&handover),
            );
            // The child is real and registered, so the caller can stop waiting
            // on the ending and race the hand-over instead. The claim travels
            // with the id because it is the same Arc the publisher reads: a
            // second lookup through the registry could miss an entry the
            // caller is racing, and a missed lookup must never default to a
            // claim that the child is already in the background. A dropped
            // receiver means the caller already returned; the child keeps
            // running either way, which is the point of the hand-over.
            drop(started.send((child_str.clone(), Arc::clone(&handle.ending))));
            // SubagentStart fires at the durable spawn boundary: the child
            // session exists, the spawn is recorded, and the run has not
            // started. It pairs with the later SubagentStop across the span up
            // to the return boundary.
            fire_subagent_start(hooks.as_ref(), parent, &child_str, &subagent_type).await;
            let ceiling = handle.cancel.clone();
            let cut_reason = Arc::clone(&handle.cut_reason);
            let liveness = Arc::clone(&handle.liveness);
            let run = finalize_child(&runtime, handle, parent, child_sid, child, hooks, task);
            let (status, summary, usage) = watch_liveness(
                run,
                liveness,
                idle_timeout,
                max_wall_clock,
                ceiling,
                cut_reason,
            )
            .await;
            Ok(SpawnOutcome::foreground(child_str, status, summary, usage))
        })
    }
}

/// Register the child's runner for a per-turn abort, record the channel that
/// hands it over when there is one, and announce the child.
///
/// The lifecycle token is registered beside the runner so a shutdown stop
/// reaches a child whose run has not started.
fn announce(
    runtime: &MultiAgentRuntime,
    handle: &ChildHandle,
    child_str: &str,
    child: ChildDescriptor,
    handover: Option<&Arc<Notify>>,
) {
    runtime.register_child(
        child_str,
        &handle.runner,
        handle.cancel.clone(),
        Some(Arc::clone(&handle.ending)),
        handover.map(Arc::clone),
    );
    announce_spawn(runtime.bus.as_ref(), child);
}
