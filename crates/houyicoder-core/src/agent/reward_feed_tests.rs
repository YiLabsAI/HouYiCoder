use super::*;
use crate::agent::ToolRegistry;
use crate::agent::auto_dream::DreamRunner;
use crate::agent::memory::{MemoryGates, MemoryRuntime, MutationLog};
use crate::agent::run_completion_tests::RefusesResults;
use crate::agent::runner_config::RunnerConfig;
use crate::agent::runner_tests::{GuardedTool, approvals_of, guarded_runner, runner_with_cfg0};
use crate::agent::{ApprovalDecision, RunOutcome};
use crate::provider::test_support::FakeProvider;
use houyicoder_api::agent_event::{
    AgentEventHandlers, MemoryChange, MemoryChangeOrigin, MemoryChangedEvent, MemoryOperation,
};
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::provider::ModelProvider;
use houyicoder_context::{MemoryEntry, MemoryError, MemoryScope, SessionId};
use houyicoder_memory::InMemoryBackend;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};
use houyicoder_session::SessionStore;
use std::sync::Mutex as StdMutex;

/// MemoryProvider stub with an empty memory_root so execute_dream
/// returns early — enough to cover the reward projection + the
/// execute_dream(Some) call without spawning a forked agent.
struct EmptyMemory;
impl MemoryProvider for EmptyMemory {
    fn add(&self, _: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
    fn update(&self, _: MemoryEntry) -> Result<(), MemoryError> {
        Ok(())
    }
    fn memory_root(&self) -> String {
        String::new()
    }
}

fn runner_with_empty_dream() -> Runner {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("x"));
    let memory: Arc<dyn MemoryProvider> = Arc::new(EmptyMemory);
    let ephemeral: Arc<dyn houyicoder_api::session::SessionLog> = store.clone();
    let dream = Arc::new(DreamRunner::new(
        ephemeral,
        Arc::clone(&provider),
        memory,
        unique_dream_cwd(),
        RunnerConfig::default(),
    ));
    let runtime = MemoryRuntime::from_parts(
        store.clone(),
        None,
        MemoryGates::new(true, true),
        None,
        Some(dream),
    );
    Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .install_memory(runtime)
}

fn unique_dream_cwd() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("houyi-reward-feed-{}-{seq}", std::process::id()))
}

/// Install a memory runtime on a runner, so a test can install a recorder on it
/// and read the notices a settling turn emits.
fn with_memory(store: Arc<SessionStore>, runner: Runner) -> Runner {
    let runtime =
        MemoryRuntime::from_parts(store, None, MemoryGates::new(false, false), None, None);
    runner.install_memory(runtime)
}

/// The paused-run shape over a memory runtime, so a seeded test can install a
/// recorder for the turn's terminal to settle.
fn guarded_memory_runner(store: Arc<SessionStore>) -> Runner {
    let runner = guarded_runner(Arc::clone(&store));
    with_memory(store, runner)
}

/// A runner with no tools over a memory runtime, for a test that drives one
/// turn and reads what the runtime emits when it settles.
fn plain_memory_runner(store: Arc<SessionStore>) -> Runner {
    let runner = Runner::new(
        store.clone(),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        runner_with_cfg0(),
    );
    with_memory(store, runner)
}

/// Install the primary recorder with a captured handler, so a test reads the
/// notices the runtime emits as a turn settles.
fn watch_memory(runner: &mut Runner) -> (Arc<MutationLog>, Arc<StdMutex<Vec<MemoryChangedEvent>>>) {
    let recorder = runner.memory.install_primary_recorder();
    (recorder, capture_notices(runner))
}

/// Read the notices the runtime emits as a turn settles. A test of the shipped
/// wiring must not call install_primary_recorder, which replaces the recorder
/// the save tool carries with one no tool writes.
fn capture_notices(runner: &mut Runner) -> Arc<StdMutex<Vec<MemoryChangedEvent>>> {
    let captured = Arc::new(StdMutex::new(Vec::<MemoryChangedEvent>::new()));
    let collected = Arc::clone(&captured);
    let mut handlers = AgentEventHandlers::default();
    handlers.set_memory_changed(Arc::new(move |event| {
        collected.lock().expect("captured").push(event);
    }));
    runner.memory.set_event_handlers(&handlers);
    captured
}

/// A runner assembled the way the composition root assembles it: install_memory
/// wires the save tool to the recorder the runtime drains, and one gated tool
/// stands beside it so a turn can pause on an approval ask.
fn shipped_memory_runner(store: Arc<SessionStore>) -> Runner {
    let provider: Arc<dyn MemoryProvider> = Arc::new(EmptyMemory);
    let runtime = MemoryRuntime::from_parts(
        store.clone(),
        Some(provider),
        MemoryGates::new(false, false),
        None,
        None,
    );
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GuardedTool::new()));
    let call = |id: &str, name: &str, input: serde_json::Value| CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let responses = vec![
        call("c1", "guarded", serde_json::json!({})),
        call(
            "c2",
            "save_memory",
            serde_json::json!({
                "key": "inline-save",
                "description": "a save the resumed leg lands",
                "source": "user",
                "content": "the resumed leg writes this",
            }),
        ),
        CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        },
    ];
    Runner::new(
        store,
        Arc::new(FakeProvider::new(responses)),
        tools,
        runner_with_cfg0(),
    )
    .install_memory(runtime)
}

#[tokio::test]
async fn test_reward_feeds_into_dream() {
    let runner = runner_with_empty_dream();
    let session = SessionId::new();
    runner
        .memory
        .fire_background(
            session,
            Some(|| {
                crate::agent::reward_snapshot::capture_reward_snapshot(
                    &runner.observability,
                    &runner.redundancy,
                )
            }),
        )
        .await;
}

#[tokio::test]
async fn test_join_dreams_no_inflight() {
    // join_dreams awaits in-flight dream JoinHandles. With no dream
    // fired (empty memory root → execute_dream returns early), there
    // are no in-flight handles — the call returns immediately.
    let runner = runner_with_empty_dream();
    runner.join_dreams(std::time::Duration::from_secs(1)).await;
    // A runner with no dream wired is also a no-op (None path).
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("x"));
    let runner_no_dream = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    runner_no_dream
        .join_dreams(std::time::Duration::from_secs(1))
        .await;
    runner_no_dream
        .memory
        .fire_background(SessionId::new(), Some(|| panic!("reward must stay lazy")))
        .await;
}

/// fire_background_memory builds the reward closure (env unset) and drives
/// the dream; the extractor is a no-op when none is wired. Covers the env
/// read and the Some branch of the background pass.
#[tokio::test]
async fn test_fire_background_drives_dream() {
    let runner = runner_with_empty_dream();
    runner.fire_background_memory(SessionId::new()).await;
}

/// fire_background_memory never drains the primary recorder. The turn's
/// settlement does, so a terminal that is not a final output still emits.
/// With a recorded save, fire_background_memory leaves the recorder full.
#[tokio::test]
async fn test_fire_background_keeps_recorder() {
    let mut runner = runner_with_empty_dream();
    let (recorder, captured) = watch_memory(&mut runner);
    recorder.record("alpha", MemoryOperation::Created, MemoryScope::Auto);
    runner.fire_background_memory(SessionId::new()).await;
    assert!(
        captured.lock().expect("captured").is_empty(),
        "fire_background_memory must not drain; the turn settlement does"
    );
    assert_eq!(
        recorder.take().len(),
        1,
        "the recorder still holds the save for the settlement"
    );
}

/// A run that ends on a terminal which is not a final output still drains the
/// primary recorder. max_turns=0 ends the run at MaxTurnsReached before any
/// model call, so fire_background_memory never runs; only the turn settlement
/// can emit. A pre-recorded save in the runtime's recorder proves the drain
/// fires.
#[tokio::test]
async fn test_run_drains_on_cap() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("x"));
    let runtime = MemoryRuntime::from_parts(
        store.clone(),
        None,
        MemoryGates::new(false, false),
        None,
        None,
    );
    let mut runner = Runner::new(
        store,
        provider,
        ToolRegistry::new(),
        RunnerConfig {
            max_turns: 0,
            ..Default::default()
        },
    )
    .install_memory(runtime);
    let (recorder, captured) = watch_memory(&mut runner);
    recorder.record("cap-save", MemoryOperation::Created, MemoryScope::Auto);
    let _result = runner.run(SessionId::new(), "remember x".into()).await;
    let events = captured.lock().expect("captured").clone();
    assert_eq!(
        events.len(),
        1,
        "the settlement drains on a max-turns terminal"
    );
    assert_eq!(events[0].origin, MemoryChangeOrigin::PrimaryAgent);
}

/// The leg a resume drives lands a save of its own, and the terminal it ends on
/// drains it. The run pauses on the gated call, the approval releases it, and
/// the resumed leg then calls the save tool inline, so the write reaches the
/// recorder through the tool the shipped runner wires rather than a seeded one.
#[tokio::test]
async fn test_resumed_leg_drains_save() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let mut runner = shipped_memory_runner(store);
    let captured = capture_notices(&mut runner);
    let session = SessionId::new();
    let paused = approvals_of(runner.run(session, "hi".into()).await.expect("run").outcome);
    let decisions: Vec<ApprovalDecision> = paused
        .iter()
        .map(|a| ApprovalDecision::approve(&a.call_id))
        .collect();
    let finished = runner.resume(session, &decisions).await.expect("resume");
    assert!(
        matches!(finished.outcome, RunOutcome::FinalOutput(_)),
        "the resumed leg runs to its own end: {:?}",
        finished.outcome
    );
    let events = captured.lock().expect("captured").clone();
    assert_eq!(events.len(), 1, "the resumed leg drains the save it landed");
    assert_eq!(events[0].origin, MemoryChangeOrigin::PrimaryAgent);
    assert_eq!(events[0].changes.len(), 1, "the notice carries one write");
    assert_eq!(
        events[0].changes[0].key, "inline-save",
        "the drained change is the key the resumed leg wrote"
    );
}

/// An aborted resume ends the turn without driving a loop, so it settles the
/// recorder too. The pause that opened the turn already drained, so no shipped
/// path leaves a change here; the seed stands in for one a released call would
/// have landed, and pins the exit that would otherwise carry it into a later
/// run.
#[tokio::test]
async fn test_aborted_resume_drains_recorder() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let mut runner = guarded_memory_runner(store);
    let (recorder, captured) = watch_memory(&mut runner);
    let session = SessionId::new();
    // The guarded call pauses the run on its approval ask, and that pause
    // drained already. The save seeded here stands in for one a released call
    // would have landed, so the abort has something to settle.
    let _paused = approvals_of(runner.run(session, "hi".into()).await.expect("run").outcome);
    recorder.record("paused-save", MemoryOperation::Created, MemoryScope::Auto);
    runner.abort();
    let _stopped = runner.resume(session, &[]).await;
    let events = captured.lock().expect("captured").clone();
    assert_eq!(
        events.len(),
        1,
        "the abort settles the turn it ends, not a later run"
    );
    assert_eq!(events[0].origin, MemoryChangeOrigin::PrimaryAgent);
}

/// The abort short-circuit reconciles the call the pause left unanswered, and
/// a log that refuses that write fails the resume before it drives a leg. The
/// turn ends there all the same, so the recorder is settled as it would be on
/// a clean abort. The seed stands in for a change a released call would have
/// landed, since no shipped path leaves one at this exit.
#[tokio::test]
async fn test_failed_abort_drains_recorder() {
    let store = Arc::new(SessionStore::new(Box::new(RefusesResults {
        inner: InMemoryBackend::new(),
    })));
    let mut runner = guarded_memory_runner(store);
    let (recorder, captured) = watch_memory(&mut runner);
    let session = SessionId::new();
    let _paused = approvals_of(runner.run(session, "hi".into()).await.expect("run").outcome);
    recorder.record("paused-save", MemoryOperation::Created, MemoryScope::Auto);
    runner.abort();
    runner
        .resume(session, &[])
        .await
        .expect_err("the refused reconciliation fails the resume");
    let events = captured.lock().expect("captured").clone();
    assert_eq!(
        events.len(),
        1,
        "the failed abort still settles the turn it ends"
    );
    assert_eq!(events[0].origin, MemoryChangeOrigin::PrimaryAgent);
}

/// A released call whose result the log refuses fails the leg part-way through
/// applying the decision. The turn ends there, so a change the leg recorded
/// would be noticed in that turn rather than in the next run. The save tool is
/// the only writer of the primary recorder and it auto-approves, so no released
/// call carries one; the seed stands in for a change a release would have left.
#[tokio::test]
async fn test_failed_resume_drains_recorder() {
    let store = Arc::new(SessionStore::new(Box::new(RefusesResults {
        inner: InMemoryBackend::new(),
    })));
    let mut runner = guarded_memory_runner(store);
    let (recorder, captured) = watch_memory(&mut runner);
    let session = SessionId::new();
    let paused = approvals_of(runner.run(session, "hi".into()).await.expect("run").outcome);
    recorder.record("paused-save", MemoryOperation::Created, MemoryScope::Auto);
    let decisions: Vec<ApprovalDecision> = paused
        .iter()
        .map(|a| ApprovalDecision::approve(&a.call_id))
        .collect();
    runner
        .resume(session, &decisions)
        .await
        .expect_err("the refused result fails the leg");
    let events = captured.lock().expect("captured").clone();
    assert_eq!(events.len(), 1, "the failed leg settles the turn it ends");
    assert_eq!(events[0].origin, MemoryChangeOrigin::PrimaryAgent);
}

/// A resume that answers none of the pending approvals re-raises the same ask,
/// so the turn pauses again rather than ending. The pause drains all the same,
/// so no abandoned pause can leave a change behind for a later turn to surface.
/// An undecided resume executes nothing, so the seed stands in for the change
/// the leg before it would have landed.
#[tokio::test]
async fn test_undecided_resume_drains_recorder() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let mut runner = guarded_memory_runner(store);
    let (recorder, captured) = watch_memory(&mut runner);
    let session = SessionId::new();
    let paused = approvals_of(runner.run(session, "hi".into()).await.expect("run").outcome);
    recorder.record("leg-save", MemoryOperation::Created, MemoryScope::Auto);
    let again = runner.resume(session, &[]).await.expect("resume");
    assert!(matches!(again.outcome, RunOutcome::Interruption(_)));
    assert_eq!(paused.len(), 1);
    let events = captured.lock().expect("captured").clone();
    assert_eq!(events.len(), 1, "the pause drains the leg before it");
    assert_eq!(events[0].origin, MemoryChangeOrigin::PrimaryAgent);
}

/// A recovered turn is a drive leg like any other, so it settles too. The seed
/// stands in for a save a leg of the recovered turn landed, so the notice is
/// emitted in the turn the redrive ends rather than carried into the next run.
/// No caller redrives a turn today, so this pins the exit rather than a path in
/// service.
#[tokio::test]
async fn test_recover_turn_drains_recorder() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let mut runner = plain_memory_runner(store);
    let (recorder, captured) = watch_memory(&mut runner);
    let session = SessionId::new();
    runner
        .append_user_input(session, "go".into())
        .await
        .expect("append user input");
    recorder.record("crashed-save", MemoryOperation::Created, MemoryScope::Auto);
    let result = runner.recover_turn(session).await.expect("redrive");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    let events = captured.lock().expect("captured").clone();
    assert_eq!(events.len(), 1, "the redrive settles the turn it ends");
    assert_eq!(events[0].origin, MemoryChangeOrigin::PrimaryAgent);
}

/// A forked run is a drive leg too, so it settles like one. A host that gave a
/// forked runner a primary recorder reads the same notice the main runner
/// emits, instead of holding the change until some later run settles it. The
/// extract builds its fork with its own recorder, so this pins the exit rather
/// than a path in service today.
#[tokio::test]
async fn test_forked_run_drains_recorder() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let mut runner = plain_memory_runner(store);
    let (recorder, captured) = watch_memory(&mut runner);
    recorder.record("fork-save", MemoryOperation::Created, MemoryScope::Auto);
    let result = runner
        .run_forked(SessionId::new(), &[], "go".into())
        .await
        .expect("forked run");
    assert!(matches!(result.outcome, RunOutcome::FinalOutput(_)));
    let events = captured.lock().expect("captured").clone();
    assert_eq!(events.len(), 1, "the forked leg settles the run it ends");
    assert_eq!(events[0].origin, MemoryChangeOrigin::PrimaryAgent);
}

/// The main runner records primary saves at call time and drains when the
/// turn settles. drain_primary_changes emits one PrimaryAgent event carrying
/// every recorded change; an empty recorder emits nothing.
#[test]
fn test_primary_recorder_drains_changes() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let mut runtime = MemoryRuntime::new(store);
    let recorder = runtime.install_primary_recorder();
    let captured = Arc::new(StdMutex::new(Vec::<MemoryChangedEvent>::new()));
    let collected = Arc::clone(&captured);
    let mut handlers = AgentEventHandlers::default();
    handlers.set_memory_changed(Arc::new(move |event| {
        collected.lock().expect("captured").push(event);
    }));
    runtime.set_event_handlers(&handlers);
    recorder.record("alpha", MemoryOperation::Created, MemoryScope::Auto);
    recorder.record("beta", MemoryOperation::Updated, MemoryScope::Auto);
    runtime.drain_primary_changes();
    let events = captured.lock().expect("captured").clone();
    assert_eq!(events.len(), 1, "one event carries both changes");
    assert_eq!(events[0].origin, MemoryChangeOrigin::PrimaryAgent);
    assert_eq!(events[0].changes.len(), 2);
    assert_eq!(
        events[0].changes[0],
        MemoryChange {
            key: "alpha".into(),
            operation: MemoryOperation::Created,
            scope: MemoryScope::Auto
        }
    );
    assert_eq!(
        events[0].changes[1],
        MemoryChange {
            key: "beta".into(),
            operation: MemoryOperation::Updated,
            scope: MemoryScope::Auto
        }
    );
    // A second drain with no new records emits nothing.
    runtime.drain_primary_changes();
    assert_eq!(
        captured.lock().expect("captured").len(),
        1,
        "an empty drain emits no event"
    );
}

#[tokio::test]
async fn test_redundancy_reminder_appends_user() {
    // Two same-tool same-input calls in one batch flag a SameBatch
    // duplicate; observe_redundancy appends a MetaUser reminder so the
    // next turn's projection serves it to the model as a system-reminder.
    let runner = runner_with_empty_dream();
    let session = SessionId::new();
    let input = serde_json::json!({"x": 1});
    let calls: Vec<(&str, &serde_json::Value)> = vec![("bash", &input), ("bash", &input)];
    runner.observe_redundancy(session, &calls).await;
    let events = runner.store.replay(session).await.expect("replay");
    assert!(
        events.iter().any(|e| matches!(
            &e.event,
            houyicoder_context::SessionEvent::MetaUser { text }
                if text.contains("bash") && text.contains("Reuse")
        )),
        "dedup reminder appended as MetaUser naming the tool + reuse cue"
    );
}

#[tokio::test]
async fn test_blind_retry_reminder_appended() {
    // A same-input call re-issued after the prior one failed (no
    // intervening write) is a blind retry. observe_redundancy appends a
    // MetaUser warning so the agent course-corrects within the query,
    // not just in the next query after the dream writes a lesson.
    let runner = runner_with_empty_dream();
    let session = SessionId::new();
    let input = serde_json::json!({"command": "cargo build"});
    // Record the prior failed call so the ledger has it as Error.
    runner
        .redundancy
        .lock()
        .expect("redundancy")
        .record("bash", &input, true);
    let calls: Vec<(&str, &serde_json::Value)> = vec![("bash", &input)];
    runner.observe_redundancy(session, &calls).await;
    let events = runner.store.replay(session).await.expect("replay");
    assert!(
        events.iter().any(|e| matches!(
            &e.event,
            houyicoder_context::SessionEvent::MetaUser { text }
                if text.contains("blind retry") && text.contains("bash")
        )),
        "blind-retry warning appended as MetaUser: {:?}",
        events.iter().map(|e| &e.event).collect::<Vec<_>>()
    );
}
