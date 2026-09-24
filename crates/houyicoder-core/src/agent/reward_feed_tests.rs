use super::*;
use crate::agent::ToolRegistry;
use crate::agent::auto_dream::DreamRunner;
use crate::agent::memory::{MemoryGates, MemoryRuntime};
use crate::agent::runner_config::RunnerConfig;
use houyicoder_api::agent_event::{
    AgentEventHandlers, MemoryChange, MemoryChangeOrigin, MemoryChangedEvent, MemoryOperation,
};
use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemoryError, MemoryScope, SessionId};
use std::collections::HashSet;
use std::sync::Mutex as StdMutex;

/// MemoryProvider stub with an empty memory_root so execute_dream
/// returns early — enough to cover the reward projection + the
/// execute_dream(Some) call without spawning a forked agent.
struct EmptyMemory;
impl MemoryProvider for EmptyMemory {
    fn recall(&self, _: &str, _: usize, _: &HashSet<String>) -> Vec<MemoryEntry> {
        Vec::new()
    }
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
    let store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(crate::provider::test_support::FakeProvider::text("x"));
    let memory: Arc<dyn MemoryProvider> = Arc::new(EmptyMemory);
    let ephemeral: Arc<dyn houyicoder_api::session::SessionLog> = store.clone();
    let dream = Arc::new(DreamRunner::new(
        ephemeral,
        Arc::clone(&provider),
        memory,
        unique_dream_cwd(),
        crate::agent::runner_config::RunnerConfig::default(),
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
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig::default(),
    )
    .install_memory(runtime)
}

fn unique_dream_cwd() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("houyi-reward-feed-{}-{seq}", std::process::id()))
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
    let store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(crate::provider::test_support::FakeProvider::text("x"));
    let runner_no_dream = Runner::new(
        store,
        provider,
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig::default(),
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
/// read and the Some branch of the run-boundary helper.
#[tokio::test]
async fn test_fire_background_drives_dream() {
    let runner = runner_with_empty_dream();
    runner.fire_background_memory(SessionId::new()).await;
}

/// fire_background_memory is the run-boundary call site that drains the
/// fire_background_memory no longer drains the primary recorder — the drain
/// moved to the run settlement so a non-final-output terminal still emits.
/// With a recorded save, fire_background_memory leaves the recorder full.
#[tokio::test]
async fn test_fire_background_keeps_recorder() {
    let mut runner = runner_with_empty_dream();
    let captured = Arc::new(StdMutex::new(Vec::<MemoryChangedEvent>::new()));
    let sink = Arc::clone(&captured);
    let mut handlers = AgentEventHandlers::default();
    handlers.set_memory_changed(Arc::new(move |event| {
        sink.lock().expect("captured").push(event);
    }));
    runner.memory.set_event_handlers(&handlers);
    let recorder = runner.memory.install_primary_recorder();
    recorder.record("alpha", MemoryOperation::Created, MemoryScope::Auto);
    runner.fire_background_memory(SessionId::new()).await;
    assert!(
        captured.lock().expect("captured").is_empty(),
        "fire_background_memory must not drain; the run settlement does"
    );
    assert_eq!(
        recorder.take().len(),
        1,
        "the recorder still holds the save for the settlement"
    );
}

/// A run that ends on a non-final-output terminal still drains the primary
/// recorder at the settlement. max_turns=0 ends the run at MaxTurnsReached
/// before any model call, so fire_background_memory never runs; only the
/// settlement drain can emit. A pre-recorded save in the runtime's recorder
/// proves the drain fires.
#[tokio::test]
async fn test_run_drains_on_cap() {
    let store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let provider: Arc<dyn houyicoder_api::provider::ModelProvider> =
        Arc::new(crate::provider::test_support::FakeProvider::text("x"));
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
    let recorder = runner.memory.install_primary_recorder();
    let captured = Arc::new(StdMutex::new(Vec::<MemoryChangedEvent>::new()));
    let sink = Arc::clone(&captured);
    let mut handlers = AgentEventHandlers::default();
    handlers.set_memory_changed(Arc::new(move |event| {
        sink.lock().expect("captured").push(event);
    }));
    runner.memory.set_event_handlers(&handlers);
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

/// The main runner records primary saves at call time and drains at the run
/// boundary. drain_primary_changes emits one PrimaryAgent event carrying
/// every recorded change; an empty recorder emits nothing.
#[test]
fn test_primary_recorder_drains_changes() {
    let store = Arc::new(houyicoder_session::SessionStore::new(Box::new(
        houyicoder_memory::InMemoryBackend::new(),
    )));
    let mut runtime = MemoryRuntime::new(store);
    let recorder = runtime.install_primary_recorder();
    let captured = Arc::new(StdMutex::new(Vec::<MemoryChangedEvent>::new()));
    let sink = Arc::clone(&captured);
    let mut handlers = AgentEventHandlers::default();
    handlers.set_memory_changed(Arc::new(move |event| {
        sink.lock().expect("captured").push(event);
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
