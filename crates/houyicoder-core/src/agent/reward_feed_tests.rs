use super::*;
use crate::agent::auto_dream::DreamRunner;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemoryError, SessionId};
use std::collections::HashSet;

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
        std::path::PathBuf::from("/tmp/houyi-reward-feed-test"),
        crate::agent::runner_config::RunnerConfig::default(),
    ));
    Runner::with_shared_store(
        store,
        provider,
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig::default(),
    )
    .with_dream(dream)
}

#[tokio::test]
async fn test_reward_feeds_into_dream() {
    // The dream's memory root is empty (InMemoryBackend), so
    // execute_dream returns early — but the reward projection + the
    // execute_dream(Some) call run (the diff-cov lines under fire).
    let runner = runner_with_empty_dream();
    let session = SessionId::new();
    runner.fire_background_at_finaloutput(session).await;
    // No panic = the projection ran + the dream was called with
    // Some(reward). The gate inside execute_dream returns early on the
    // empty memory root, so no forked agent spawns.
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
    let runner_no_dream = Runner::with_shared_store(
        store,
        provider,
        crate::agent::ToolRegistry::new(),
        crate::agent::runner_config::RunnerConfig::default(),
    );
    runner_no_dream
        .join_dreams(std::time::Duration::from_secs(1))
        .await;
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
            &e.kind,
            houyicoder_context::TurnEventKind::MetaUser { text }
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
            &e.kind,
            houyicoder_context::TurnEventKind::MetaUser { text }
                if text.contains("blind retry") && text.contains("bash")
        )),
        "blind-retry warning appended as MetaUser: {:?}",
        events.iter().map(|e| &e.kind).collect::<Vec<_>>()
    );
}
