//! Handing a running foreground child over to the background.

use super::auto_background::GatedProvider;
use super::*;
use houyicoder_api::spawn::BackgroundLaunch;
use houyicoder_core::agent::multi_agent::ending::EndingClaim;

/// Handing over a running foreground child flips the mode its ending is
/// published with and releases the waiting spawn call. A second hand-over is a
/// no-op: the spawn call is no longer waiting, and the ending already notifies,
/// so reporting another transition would tell the caller it freed a turn it did
/// not.
#[test]
fn test_handover_is_idempotent() {
    let bus = Arc::new(AgentBus::new());
    let (runtime, _store, _parent_sid) = runtime_with(
        Arc::new(FakeProvider::text("ok")),
        Some(bus),
        ToolRegistry::new(),
    );
    let runner = Arc::new(Runner::new(
        runtime.store.clone(),
        Arc::new(FakeProvider::text("ok")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    let ending = Arc::new(EndingClaim::new(ChildRunMode::Foreground));
    let handover = Arc::new(Notify::new());
    runtime.register_child(
        "c1",
        &runner,
        CancellationToken::new(),
        Some(Arc::clone(&ending)),
        Some(Arc::clone(&handover)),
    );
    assert!(
        runtime.background_child("c1"),
        "a live foreground child hands over"
    );
    assert!(
        ending.notifies_parent(),
        "the ending now notifies the parent"
    );
    assert!(
        !runtime.background_child("c1"),
        "a second hand-over reports no transition"
    );
    assert!(
        !runtime.background_child("unknown"),
        "an unknown child reports no transition"
    );
    // A child registered without a hand-over channel (already in the
    // background, or started by a caller with no turn to release) cannot be
    // handed over.
    runtime.register_child("c2", &runner, CancellationToken::new(), None, None);
    assert!(
        !runtime.background_child("c2"),
        "no channel means no hand-over"
    );
}

/// A runtime with no bus cannot deliver the notification a hand-over
/// promises, so it refuses rather than freeing a turn whose answer nobody
/// carries.
#[test]
fn test_handover_without_channel_refused() {
    let (runtime, _store, _parent_sid) = runtime_with_text_child("ok");
    let runner = Arc::new(Runner::new(
        runtime.store.clone(),
        Arc::new(FakeProvider::text("ok")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    ));
    let ending = Arc::new(EndingClaim::new(ChildRunMode::Foreground));
    let handover = Arc::new(Notify::new());
    runtime.register_child(
        "c1",
        &runner,
        CancellationToken::new(),
        Some(Arc::clone(&ending)),
        Some(Arc::clone(&handover)),
    );
    assert!(
        !runtime.background_child("c1"),
        "a turn with no notification channel keeps waiting"
    );
    assert!(
        !ending.notifies_parent(),
        "the ending still belongs to the caller that waits on it"
    );
}

/// A turn that hands its child over stops waiting, and its own stop no longer
/// reaches that child.
///
/// The two lifecycles are separate: the turn's token ends the turn, the
/// child's token ends the child. A stop on one must not reach the other, and a
/// hand-over is the moment the turn gives up every claim on the child.
#[tokio::test]
async fn test_handover_frees_the_turn() {
    let bus = Arc::new(AgentBus::new());
    let (provider, entered) = StallForeverProvider::new();
    let (runtime, _store, parent_sid) =
        runtime_with(provider, Some(Arc::clone(&bus)), ToolRegistry::new());
    let turn = CancellationToken::new();
    let ctx = ToolCtx::new("c1")
        .with_session(parent_sid)
        .with_cancel(turn.clone());
    let mut announced = bus.subscribe(spawned_topic());
    let mut completed = bus.subscribe(global_completed_topic());
    let mut call = runtime.spawn(&ctx, SpawnArgs::new("explore", "find auth", "find auth"));
    let child = tokio::select! {
        message = announced.recv() => match message {
            Ok(BusMessage::Spawned { child }) => child,
            other => panic!("the boundary must announce the child: {other:?}"),
        },
        result = &mut call => panic!("the call returned before the boundary: {result:?}"),
    };
    entered
        .await
        .expect("the child entered its first model call");
    assert!(
        runtime.background_child(&child.agent_id),
        "a running foreground child can be handed over"
    );
    let launch = call.await.expect("the spawn reports the launch");
    assert!(
        launch.status.is_none(),
        "the turn is released with a launch, not with an ending"
    );
    assert_eq!(
        launch.background,
        Some(BackgroundLaunch::HandedOver),
        "the launch names the reader's hand-over, not a background request"
    );
    assert_eq!(
        launch.child_session_id, child.agent_id,
        "the launch names the child that keeps running"
    );
    // The turn no longer waits on the child, so stopping the turn must leave it
    // alone. A short wait is the negative assertion: nothing may arrive.
    turn.cancel();
    assert!(
        timeout(Duration::from_millis(200), completed.recv())
            .await
            .is_err(),
        "stopping the turn must not end a child it handed over"
    );
    // The child is still live, and stopping it reaches the parent as a
    // background ending.
    assert!(
        runtime.kill_child(&child.agent_id),
        "the handed-over child is still running"
    );
    let message = timeout(Duration::from_secs(5), completed.recv())
        .await
        .expect("a stopped child publishes its ending")
        .expect("the bus stays open");
    assert!(
        matches!(
            &message,
            BusMessage::Completed { child, .. } if child.run_mode == ChildRunMode::Background
        ),
        "the ending reaches the parent as a background ending: {message:?}"
    );
}

/// A child's own ceiling stops the child and nothing else.
///
/// The ceiling ends the child's run and produces a report for the turn, so the
/// turn has to survive it: a turn cancelled by its own child's deadline would
/// lose the very report that deadline produced.
#[tokio::test]
async fn test_ceiling_spares_the_turn() {
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new_with_delay(
        vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "unreachable".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }],
        5_000,
    ));
    let (runtime, _store, parent_sid) = runtime_with(provider, None, ToolRegistry::new());
    let turn = CancellationToken::new();
    let ctx = ToolCtx::new("c1")
        .with_session(parent_sid)
        .with_cancel(turn.clone());
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.timeout_secs = Some(1);

    let outcome = runtime.spawn(&ctx, args).await.expect("the spawn runs");

    assert_eq!(
        outcome.status.as_deref(),
        Some("timed_out"),
        "the ceiling is named as the thing that ended the child"
    );
    assert!(
        !turn.is_cancelled(),
        "a child's own ceiling must not stop the turn that started it"
    );
}

/// A child started in the background obeys the ceiling its caller asked for,
/// and the fleet is told the ceiling ended it rather than a plain interrupt:
/// the caller is gone, so this status is the only account of the ending the
/// parent and the user get.
#[tokio::test]
async fn test_detached_child_respects_ceiling() {
    let bus = Arc::new(AgentBus::new());
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new_with_delay(
        vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "unreachable".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }],
        5_000,
    ));
    let (runtime, _store, parent_sid) =
        runtime_with(provider, Some(Arc::clone(&bus)), ToolRegistry::new());
    let mut completed = bus.subscribe(global_completed_topic());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut args = SpawnArgs::new("explore", "task", "task");
    args.run_in_background = true;
    args.timeout_secs = Some(1);

    let launch = runtime
        .spawn(&ctx, args)
        .await
        .expect("a background child launches");
    assert!(launch.status.is_none(), "the caller is not made to wait");

    let message = timeout(Duration::from_secs(5), completed.recv())
        .await
        .expect("the ceiling ends a child started in the background")
        .expect("the bus stays open");
    assert!(
        matches!(
            message,
            BusMessage::Completed {
                status: CompletionStatus::TimedOut,
                ..
            }
        ),
        "the ceiling reaches the fleet as a timeout, not a kill: {message:?}"
    );
}

/// A finished child leaves no admission behind.
///
/// The admission slot is what a later shutdown waits on, so one left behind
/// makes the shutdown wait out a child that is no longer there, and a long
/// session collects one per delegation.
#[tokio::test]
async fn test_finished_child_releases_admission() {
    let (runtime, _store, parent_sid) = runtime_with_text_child("ok");
    let ctx = ToolCtx::new("c1").with_session(parent_sid);

    runtime
        .spawn(&ctx, SpawnArgs::new("explore", "task", "task"))
        .await
        .expect("the spawn runs");

    let live = runtime
        .running
        .lock()
        .expect("the running set locks")
        .children
        .len();
    assert_eq!(
        live, 0,
        "a finished child must not leave an admission behind"
    );
}

/// A provider that reports the call, waits for the test's go-ahead, then fails
/// the call. It lets a test hand the child over first and fail it afterwards,
/// which is the order a real child takes when its provider breaks mid-run.
struct FailOnRelease {
    entered: Mutex<Option<Sender<()>>>,
    release: Mutex<Option<Receiver<()>>>,
}

impl FailOnRelease {
    fn new() -> (Arc<Self>, Receiver<()>, Sender<()>) {
        let (entered, entered_rx) = channel();
        let (release_tx, release) = channel();
        (
            Arc::new(Self {
                entered: Mutex::new(Some(entered)),
                release: Mutex::new(Some(release)),
            }),
            entered_rx,
            release_tx,
        )
    }
}

impl ModelProvider for FailOnRelease {
    fn complete(
        &self,
        _req: CompletionRequest,
    ) -> PFut<'_, Result<CompletionResponse, ProviderError>> {
        Box::pin(pending())
    }

    fn stream(&self, _req: CompletionRequest) -> PStream<'_, Result<LlmEvent, ProviderError>> {
        let entered = self.entered.lock().expect("entered lock").take();
        let release = self.release.lock().expect("release lock").take();
        Box::pin(once(async move {
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            if let Some(release) = release {
                let _ = release.await;
            }
            Err(ProviderError::InvalidRequest(
                "the child was asked to fail".into(),
            ))
        }))
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
}

/// A tool that waits for the test's go-ahead and then panics, so a child dies
/// the way a bug in a tool kills it: by unwinding out of the run, before any
/// terminal is published.
struct PanicsOnRelease {
    gate: Arc<Notify>,
}

impl Tool for PanicsOnRelease {
    fn name(&self) -> &str {
        "boom"
    }

    fn description(&self) -> &str {
        "panics once the test releases it"
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }

    fn execute(&self, _ctx: ToolCtx, _input: Value) -> PFut<'_, Result<Value, ToolError>> {
        Box::pin(async move {
            self.gate.notified().await;
            panic!("the tool blew up");
        })
    }
}

/// A child that dies after the turn released it still reaches the parent.
///
/// A handed-over child's ending is reported as a background ending, because
/// that is what notifies. A run that unwinds publishes no terminal of its own,
/// so the panic shape is the one that reaches this reporting arm.
#[tokio::test]
async fn test_handover_panic_notifies() {
    let bus = Arc::new(AgentBus::new());
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::from_outputs(vec![vec![
        OutputItem::ToolCall {
            id: "tc1".into(),
            name: "boom".into(),
            input: serde_json::json!({}),
        },
    ]]));
    let gate = Arc::new(Notify::new());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(PanicsOnRelease {
        gate: Arc::clone(&gate),
    }));
    let (runtime, _store, parent_sid) = runtime_with(provider, Some(Arc::clone(&bus)), tools);
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut announced = bus.subscribe(spawned_topic());
    let mut completed = bus.subscribe(global_completed_topic());
    let mut call = runtime.spawn(&ctx, SpawnArgs::new("explore", "find auth", "find auth"));
    let child = tokio::select! {
        message = announced.recv() => match message {
            Ok(BusMessage::Spawned { child }) => child,
            other => panic!("the boundary must announce the child: {other:?}"),
        },
        result = &mut call => panic!("the call returned before the boundary: {result:?}"),
    };
    assert!(
        runtime.background_child(&child.agent_id),
        "a running foreground child can be handed over"
    );
    call.await.expect("the spawn reports the launch");

    gate.notify_one();
    let message = timeout(Duration::from_secs(5), completed.recv())
        .await
        .expect("a child that died publishes its ending")
        .expect("the bus stays open");
    assert!(
        matches!(
            &message,
            BusMessage::Completed {
                child,
                status: CompletionStatus::Failed,
                ..
            } if child.run_mode == ChildRunMode::Background
        ),
        "a handed-over child that died reports a background failure: {message:?}"
    );
}

/// A child that fails after the turn released it still reaches the parent.
///
/// The ending is reported with the mode the child has at that moment, not the
/// one it started in: the parent stopped waiting at the hand-over, so a
/// foreground ending here would notify nobody.
#[tokio::test]
async fn test_handover_failure_notifies() {
    let bus = Arc::new(AgentBus::new());
    let (provider, entered, release) = FailOnRelease::new();
    let (runtime, _store, parent_sid) =
        runtime_with(provider, Some(Arc::clone(&bus)), ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut announced = bus.subscribe(spawned_topic());
    let mut completed = bus.subscribe(global_completed_topic());
    let mut call = runtime.spawn(&ctx, SpawnArgs::new("explore", "find auth", "find auth"));
    let child = tokio::select! {
        message = announced.recv() => match message {
            Ok(BusMessage::Spawned { child }) => child,
            other => panic!("the boundary must announce the child: {other:?}"),
        },
        result = &mut call => panic!("the call returned before the boundary: {result:?}"),
    };
    entered
        .await
        .expect("the child entered its first model call");
    assert!(
        runtime.background_child(&child.agent_id),
        "a running foreground child can be handed over"
    );
    call.await.expect("the spawn reports the launch");

    let _ = release.send(());
    let message = timeout(Duration::from_secs(5), completed.recv())
        .await
        .expect("a failed child publishes its ending")
        .expect("the bus stays open");
    assert!(
        matches!(
            &message,
            BusMessage::Completed {
                child,
                status: CompletionStatus::Failed,
                ..
            } if child.run_mode == ChildRunMode::Background
        ),
        "a handed-over child that fails reports a background failure: {message:?}"
    );
}

/// A hand-over taken the instant the child is announced keeps the child alive
/// even when the waiting turn is dropped before it learns the child's id.
///
/// The child binds its ending claim before announcing itself, so the wait's
/// drop reads the hand-over rather than stopping a child the reader just
/// freed. Binding any later leaves a window where the reader has been told
/// the turn is free and the drop still kills the child.
#[tokio::test]
async fn test_early_handover_spares_child() {
    let bus = Arc::new(AgentBus::new());
    let (provider, entered, mut cut) = GatedProvider::new();
    let gate = provider.gate();
    let (runtime, _store, parent_sid) =
        runtime_with(provider, Some(Arc::clone(&bus)), ToolRegistry::new());
    let ctx = ToolCtx::new("c1").with_session(parent_sid);
    let mut announced = bus.subscribe(spawned_topic());
    let mut completed = bus.subscribe(global_completed_topic());
    let mut call = runtime.spawn(&ctx, SpawnArgs::new("explore", "find auth", "find auth"));
    let child = tokio::select! {
        message = announced.recv() => match message {
            Ok(BusMessage::Spawned { child }) => child,
            other => panic!("the boundary must announce the child: {other:?}"),
        },
        result = &mut call => panic!("the call returned before the boundary: {result:?}"),
    };
    assert!(
        runtime.background_child(&child.agent_id),
        "the reader can take the child over the moment it is announced"
    );
    // The turn is dropped while it may still be waiting for the child's id:
    // this is the drop the claim has to spare.
    drop(call);
    timeout(Duration::from_secs(5), entered)
        .await
        .expect("the dropped wait leaves the child running")
        .expect("the child's model call started");
    assert!(
        timeout(Duration::from_millis(200), &mut cut).await.is_err(),
        "the dropped wait must not stop a child it handed over"
    );
    gate.notify_one();
    let message = timeout(Duration::from_secs(5), completed.recv())
        .await
        .expect("the freed child publishes its ending")
        .expect("the bus stays open");
    assert!(
        matches!(
            &message,
            BusMessage::Completed {
                child,
                status: CompletionStatus::Completed,
                ..
            } if child.run_mode == ChildRunMode::Background
        ),
        "the child finishes on its own and reports a background ending: {message:?}"
    );
}
