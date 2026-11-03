//! The foreground wait budget: when a slow delegation is sent to the
//! background, and when a fast one is not.

use super::*;
use crate::composition::multi_agent::spawn_exec::handover::{
    ElapsedBudget, resolve_elapsed_budget,
};
use futures::StreamExt;
use futures::stream::{iter, once};
use houyicoder_core::agent::multi_agent::bus_types::completed_topic;
use houyicoder_core::agent::multi_agent::ending::EndingClaim;
use tokio::time::advance;

/// Signals when the future that owns it ends, whether the stream completes or
/// is dropped. A stop drops a run's model call, so the tests read the signal
/// while the gate is still closed: a signal there is the stop the turn release
/// must not have sent.
struct StreamDropGuard(Option<Sender<()>>);

impl Drop for StreamDropGuard {
    fn drop(&mut self) {
        if let Some(cut) = self.0.take() {
            let _ = cut.send(());
        }
    }
}

/// Enters its model call, signals the test, then waits for the gate before
/// answering. Holding the answer behind the gate is what shows whether the
/// turn release left the child alone: the answer lands only if the run was
/// still alive when the gate opened, and the drop report says when a stop
/// reached it first. Shared with the hand-over tests, which ask the same
/// question about a different release.
pub(super) struct GatedProvider {
    entered: Mutex<Option<Sender<()>>>,
    cut: Mutex<Option<Sender<()>>>,
    gate: Arc<Notify>,
}

impl GatedProvider {
    pub(super) fn new() -> (Arc<Self>, Receiver<()>, Receiver<()>) {
        let (entered, entered_rx) = channel();
        let (cut, cut_rx) = channel();
        (
            Arc::new(Self {
                entered: Mutex::new(Some(entered)),
                cut: Mutex::new(Some(cut)),
                gate: Arc::new(Notify::new()),
            }),
            entered_rx,
            cut_rx,
        )
    }

    pub(super) fn gate(&self) -> Arc<Notify> {
        Arc::clone(&self.gate)
    }
}

impl ModelProvider for GatedProvider {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(pending())
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let entered = self.entered.lock().expect("entered lock").take();
        let cut = StreamDropGuard(self.cut.lock().expect("cut lock").take());
        let gate = Arc::clone(&self.gate);
        let events = once(async move {
            let _cut = cut;
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            gate.notified().await;
            Ok(LlmEvent::StepStart { index: 0 })
        })
        .chain(iter([
            Ok(LlmEvent::TextStart { id: "t0".into() }),
            Ok(LlmEvent::TextDelta {
                id: "t0".into(),
                text: "the answer".into(),
            }),
            Ok(LlmEvent::TextEnd { id: "t0".into() }),
            Ok(LlmEvent::StepFinish {
                index: 0,
                reason: "stop".into(),
                usage: None,
            }),
            Ok(LlmEvent::Finish {
                reason: "stop".into(),
                usage: None,
            }),
        ]));
        Box::pin(events)
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A foreground delegation that outlives its wait budget is sent to the
/// background and the turn is released: the outcome tells the model not to
/// guess the unfinished work, and the child keeps running so its ending
/// arrives later as a notification.
#[tokio::test]
async fn test_slow_child_auto_backgrounds() {
    let (provider, entered) = StallForeverProvider::new();
    let (runtime, _store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let runtime = runtime.with_delegation_wait_ms(150);
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "slow task", "slow task");

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");

    assert!(
        outcome.status.is_none(),
        "the turn is released without a terminal status: {:?}",
        outcome.status
    );
    let summary = outcome
        .summary
        .expect("the outcome carries the instruction");
    assert!(
        summary.contains("now running in the background"),
        "the model is told the child is in the background: {summary}"
    );
    assert!(
        summary.contains(&outcome.child_session_id),
        "the instruction names the child so the model can steer or resume it: {summary}"
    );
    entered.await.expect("the child entered its model call");
    runtime.kill_child(&outcome.child_session_id);
    runtime.join_children().await;
}

/// A child sent to the background because the turn stopped waiting keeps
/// running to its own ending: once its model call completes it publishes the
/// work as done. The turn's stop link is released with the child rather than
/// dropped armed, so the return that frees the turn does not cancel the work
/// it just handed over.
#[tokio::test]
async fn test_released_child_still_finishes() {
    let bus = Arc::new(AgentBus::new());
    let (provider, entered, mut cut) = GatedProvider::new();
    let gate = provider.gate();
    let (runtime, _store, parent_sid) =
        runtime_with(provider, Some(Arc::clone(&bus)), ToolRegistry::new());
    let runtime = runtime.with_delegation_wait_ms(150);
    let ctx = ToolCtx::new("c1").with_session(parent_sid);

    let outcome = runtime
        .spawn(&ctx, SpawnArgs::new("explore", "slow task", "slow task"))
        .await
        .expect("the spawn runs");
    assert!(
        outcome.status.is_none(),
        "the turn is released before the child ends: {:?}",
        outcome.status
    );
    entered.await.expect("the child entered its model call");
    // A stop cancels the child's lifecycle token, which wakes the child task;
    // yielding lets it run before the gate opens. A release that left the link
    // armed drops the model call here and reports the stop.
    yield_now().await;
    cut.try_recv()
        .expect_err("the turn release must not stop the child it handed over");
    // Subscribed before the gate opens, so the ending cannot land unwatched:
    // the child is parked inside its model call until the gate opens.
    let mut done = bus.subscribe(&completed_topic(&outcome.child_session_id));
    gate.notify_one();

    let ending = timeout(Duration::from_secs(2), done.recv())
        .await
        .expect("the released child keeps running to its ending")
        .expect("the child publishes its ending");
    match ending {
        BusMessage::Completed {
            status, summary, ..
        } => {
            assert_eq!(
                status.as_str(),
                "completed",
                "the child finishes its own work, not a stop: {summary}"
            );
            assert!(
                summary.contains("the answer"),
                "the child's answer survived the turn release: {summary}"
            );
        }
        other => panic!("the child must publish an ending: {other:?}"),
    }
    runtime.join_children().await;
}

/// A budget that elapses while the child is still in its finalize tail does
/// not replace the answer the run already reached. The child claimed the
/// ending for the caller when it reported its terminal, so the budget cannot
/// claim it for the notification stream afterwards; the wait keeps waiting and
/// returns the child's own result.
///
/// The clock is paused so the budget elapses exactly while the tail is parked,
/// and the call is polled once before the tail is released: a wait that
/// answered the budget before checking the claim would return a launch here.
#[tokio::test(start_paused = true)]
async fn test_budget_keeps_answer() {
    struct ParkedStopHook {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }
    impl HookFire for ParkedStopHook {
        fn fire(&self, event: HookEventKind, _payload: HookFirePayload) -> PFut<'_, ()> {
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            Box::pin(async move {
                if event == HookEventKind::SubagentStop {
                    entered.notify_one();
                    release.notified().await;
                }
            })
        }
    }
    // The child publishes through a bus, which is what lets its terminal claim
    // the ending for the caller before the finalize tail starts.
    let bus = Arc::new(AgentBus::new());
    let (runtime, _store, parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("the answer")),
        Some(bus),
        ToolRegistry::new(),
    );
    let runtime = runtime.with_delegation_wait_ms(150);
    let hook = Arc::new(ParkedStopHook {
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
    });
    let ctx = ToolCtx::new("c1")
        .with_session(parent_sid)
        .with_hook_fire(hook.clone() as Arc<dyn HookFire>);
    let mut call = Box::pin(runtime.spawn(&ctx, SpawnArgs::new("explore", "task", "task")));
    tokio::select! {
        _ = hook.entered.notified() => {}
        result = &mut call => panic!("the call returned before the tail parked: {result:?}"),
    }
    // The child reached its terminal and claimed the ending for this caller.
    // Elapse the budget with the tail still parked, then poll the call once so
    // the elapsed budget decides against the claim before the child can finish.
    advance(Duration::from_millis(200)).await;
    tokio::select! {
        biased;
        result = &mut call => panic!("the call left an earned answer behind: {result:?}"),
        _ = yield_now() => {}
    }
    hook.release.notify_one();
    let outcome = call.await.expect("the spawn runs");
    assert_eq!(
        outcome.status.as_deref(),
        Some("completed"),
        "the child's terminal survives the elapsed budget"
    );
    assert_eq!(
        outcome.summary.as_deref(),
        Some("the answer"),
        "the turn keeps the answer it already paid for"
    );
}

/// A child the wait budget sent to the background publishes its ending on the
/// notification stream with the background run mode, so a watcher learns the
/// child ended without waiting for it in a turn.
#[tokio::test]
async fn test_auto_background_notifies_parent() {
    let bus = Arc::new(AgentBus::new());
    let (provider, entered, _cut) = GatedProvider::new();
    let gate = provider.gate();
    let (runtime, _store, parent_sid) =
        runtime_with(provider, Some(Arc::clone(&bus)), ToolRegistry::new());
    let runtime = runtime.with_delegation_wait_ms(150);
    let ctx = ToolCtx::new("c1").with_session(parent_sid);

    let outcome = runtime
        .spawn(&ctx, SpawnArgs::new("explore", "slow task", "slow task"))
        .await
        .expect("the spawn runs");
    assert!(
        outcome.background.is_some(),
        "the wait budget releases the turn"
    );
    entered.await.expect("the child entered its model call");
    // Subscribed before the gate opens, so the ending cannot land unwatched.
    let mut endings = bus.subscribe(global_completed_topic());
    gate.notify_one();

    let ending = timeout(Duration::from_secs(2), endings.recv())
        .await
        .expect("the released child publishes its ending")
        .expect("the ending arrives");
    match ending {
        BusMessage::Completed { child, status, .. } => {
            assert_eq!(status.as_str(), "completed", "the child finishes its work");
            assert_eq!(
                child.run_mode,
                ChildRunMode::Background,
                "the ending reaches the notification stream, not a caller that stopped waiting"
            );
        }
        other => panic!("the child must publish an ending: {other:?}"),
    }
    runtime.join_children().await;
}

/// An elapsed budget with no claim on the ending takes it for the notification
/// stream, which is what releases the turn.
#[test]
fn test_budget_claims_open_ending() {
    let ending = EndingClaim::new(ChildRunMode::Foreground);
    assert_eq!(resolve_elapsed_budget(&ending), ElapsedBudget::Release);
    assert!(ending.notifies_parent());
}

/// A run that already claimed the ending for its caller keeps the budget from
/// claiming it for a notification nobody would send.
#[test]
fn test_budget_defers_to_run() {
    let ending = EndingClaim::new(ChildRunMode::Foreground);
    assert!(ending.claim_caller(), "the run reached its terminal");
    assert_eq!(resolve_elapsed_budget(&ending), ElapsedBudget::WaitForRun);
}

/// A reader's hand-over claims the ending before it raises its signal, so a
/// timer that loses the claim to it must still release the turn. Treating that
/// loss as the run's own means the turn blocks until the child ends although
/// the reader already freed it.
#[test]
fn test_budget_yields_to_handover() {
    let ending = EndingClaim::new(ChildRunMode::Foreground);
    assert!(ending.claim_notification(), "the hand-over claims first");
    assert_eq!(
        resolve_elapsed_budget(&ending),
        ElapsedBudget::HandedOver,
        "the turn is released by the side that claimed it"
    );
}

/// A foreground delegation that finishes inside its wait budget returns the
/// child's own ending, not a background launch, so the turn keeps the answer
/// it already paid for.
#[tokio::test]
async fn test_fast_child_returns_foreground() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("the answer");
    let runtime = runtime.with_delegation_wait_ms(5_000);
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let args = SpawnArgs::new("explore", "fast task", "fast task");

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");

    assert_eq!(
        outcome.status.as_deref(),
        Some("completed"),
        "a child that finishes inside the budget keeps its own ending"
    );
    assert_eq!(
        outcome.summary.as_deref(),
        Some("the answer"),
        "the turn keeps the child's answer, not a background instruction"
    );
}
