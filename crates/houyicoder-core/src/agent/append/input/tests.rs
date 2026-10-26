//! Tests for this module.

use std::sync::Arc;

use houyicoder_context::{
    DescriptorUpdate, PromptCoverage, SessionDescriptor, SessionDescriptorError,
    SessionDescriptorStore, SessionEvent, SessionId, SessionProvenance,
};
use houyicoder_memory::{InMemoryBackend, InMemoryDescriptorStore};
use houyicoder_provider::FakeProvider;
use houyicoder_session::SessionStore;

use super::*;
use crate::agent::ToolRegistry;
use crate::agent::runner_config::RunnerConfig;

fn runner(descriptor_store: Arc<impl SessionDescriptorStore + 'static>) -> Runner {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Runner::new(
        store,
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            ..RunnerConfig::default()
        },
    )
    .with_descriptor_store(descriptor_store)
}

fn fresh(session: SessionId) -> Arc<InMemoryDescriptorStore> {
    let store = Arc::new(InMemoryDescriptorStore::new());
    let descriptor =
        SessionDescriptor::delegated_child(SessionProvenance::Fresh, 0, PromptCoverage::Complete);
    store.write_descriptor(session, &descriptor).unwrap();
    store
}

/// A store whose every write fails, so a test can hold the best-effort
/// contract: the durable append stands and only the display metadata is
/// lost.
struct FailingDescriptorStore;

impl SessionDescriptorStore for FailingDescriptorStore {
    fn read_descriptor(&self, _session: SessionId) -> Option<SessionDescriptor> {
        None
    }

    fn write_descriptor(
        &self,
        _session: SessionId,
        _descriptor: &SessionDescriptor,
    ) -> Result<(), SessionDescriptorError> {
        Err(SessionDescriptorError("disk full".into()))
    }

    fn write_descriptor_if_absent(
        &self,
        _session: SessionId,
        _descriptor: &SessionDescriptor,
    ) -> Result<bool, SessionDescriptorError> {
        Err(SessionDescriptorError("disk full".into()))
    }

    fn update_descriptor(
        &self,
        _session: SessionId,
        _edit: &mut dyn FnMut(&mut SessionDescriptor),
    ) -> Result<DescriptorUpdate, SessionDescriptorError> {
        Err(SessionDescriptorError("disk full".into()))
    }

    fn delete_descriptor(&self, _session: SessionId) {}
}

#[tokio::test]
async fn test_user_input_records_prompt() {
    let session = SessionId::new();
    let descriptors = fresh(session);
    let runner = runner(Arc::clone(&descriptors));

    runner
        .append_user_input(session, "fix the login test".to_string())
        .await
        .unwrap();
    runner
        .append_user_input(session, "now update the changelog".to_string())
        .await
        .unwrap();
    // The descriptor write outlives the append, so reading without settling
    // would race it.
    runner.settle_prompt_writes().await;

    let descriptor = descriptors.read_descriptor(session).unwrap();
    assert_eq!(
        descriptor.first_prompt.as_ref().unwrap().text,
        "fix the login test"
    );
    assert_eq!(
        descriptor.last_prompt.as_ref().unwrap().text,
        "now update the changelog"
    );
    let events = runner.store.replay(session).await.unwrap();
    let first_user_input = events
        .iter()
        .find(|e| matches!(e.event, SessionEvent::UserInput { .. }))
        .unwrap();
    assert_eq!(
        descriptor.first_prompt.as_ref().unwrap().event_id,
        first_user_input.id,
        "the record names the event it was read from"
    );
}

#[tokio::test]
async fn test_greeting_records_no_prompt() {
    let session = SessionId::new();
    let descriptors = fresh(session);
    let runner = runner(Arc::clone(&descriptors));

    runner
        .append_user_input(session, "hi".to_string())
        .await
        .unwrap();
    runner.settle_prompt_writes().await;

    let descriptor = descriptors.read_descriptor(session).unwrap();
    assert!(descriptor.first_prompt.is_none());
    assert!(descriptor.last_prompt.is_none());
}

#[tokio::test]
async fn test_mid_turn_records_last() {
    let session = SessionId::new();
    let descriptors = fresh(session);
    let runner = runner(Arc::clone(&descriptors));

    runner
        .append_user_input(session, "start the refactor".to_string())
        .await
        .unwrap();
    runner
        .append_mid_turn_input(session, "also keep the old name".to_string(), Some(1))
        .await
        .unwrap();
    runner.settle_prompt_writes().await;

    let descriptor = descriptors.read_descriptor(session).unwrap();
    assert_eq!(
        descriptor.last_prompt.as_ref().unwrap().text,
        "also keep the old name"
    );
}

/// A message another agent sent reaches the log by the interjection path, but
/// it states that agent's words rather than the user's, so it leaves the
/// recorded prompts alone.
#[tokio::test]
async fn test_agent_message_skips_prompt() {
    let session = SessionId::new();
    let descriptors = fresh(session);
    let runner = runner(Arc::clone(&descriptors));

    runner
        .append_user_input(session, "start the refactor".to_string())
        .await
        .unwrap();
    runner
        .append_mid_turn_input(session, "child finished: tests pass".to_string(), None)
        .await
        .unwrap();
    runner.settle_prompt_writes().await;

    let descriptor = descriptors.read_descriptor(session).unwrap();
    assert_eq!(
        descriptor.last_prompt.as_ref().unwrap().text,
        "start the refactor",
        "another agent's message does not outrank the user's own words"
    );
    let events = runner.store.replay(session).await.unwrap();
    assert_eq!(events.len(), 2, "the message still reaches the log");
}

/// A session with no descriptor (written before descriptors existed, or
/// whose descriptor was removed) still keeps its log: the append stands and
/// nothing invents a descriptor on this path.
#[tokio::test]
async fn test_input_without_descriptor_logs() {
    let session = SessionId::new();
    let descriptors = Arc::new(InMemoryDescriptorStore::new());
    let runner = runner(Arc::clone(&descriptors));

    runner
        .append_user_input(session, "fix the login test".to_string())
        .await
        .expect("a missing descriptor does not fail the append");
    runner.settle_prompt_writes().await;

    assert!(descriptors.read_descriptor(session).is_none());
    let events = runner.store.replay(session).await.unwrap();
    assert_eq!(events.len(), 1, "the input is still in the log");
}

/// A descriptor write that fails (full disk, unreadable root) is diagnostic
/// only: the input already reached the durable log, and failing the turn
/// over display metadata would lose the user's words.
#[tokio::test]
async fn test_descriptor_failure_keeps_input() {
    let session = SessionId::new();
    let runner = runner(Arc::new(FailingDescriptorStore));

    runner
        .append_user_input(session, "fix the login test".to_string())
        .await
        .expect("a failed descriptor write does not fail the append");

    let events = runner.store.replay(session).await.unwrap();
    assert_eq!(events.len(), 1, "the input is still in the log");
}

#[tokio::test]
async fn test_unknown_coverage_skips_first() {
    let session = SessionId::new();
    let descriptors = Arc::new(InMemoryDescriptorStore::new());
    let mut descriptor =
        SessionDescriptor::delegated_child(SessionProvenance::Fresh, 0, PromptCoverage::Complete);
    descriptor.prompt_coverage = PromptCoverage::Unknown;
    descriptors.write_descriptor(session, &descriptor).unwrap();
    let runner = runner(Arc::clone(&descriptors));

    runner
        .append_user_input(session, "a later task".to_string())
        .await
        .unwrap();
    runner.settle_prompt_writes().await;

    let descriptor = descriptors.read_descriptor(session).unwrap();
    assert!(
        descriptor.first_prompt.is_none(),
        "an unread log has no known first prompt"
    );
    assert_eq!(
        descriptor.last_prompt.as_ref().unwrap().text,
        "a later task"
    );
}
