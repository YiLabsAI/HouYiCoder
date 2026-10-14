//! Tests for this module.

use super::super::ToolRegistry;
use super::super::runner_config::RunnerConfig;
use super::{QueuedInput, Runner};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;

fn bare_runner() -> Runner {
    let store = std::sync::Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    Runner::new(
        store,
        std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("ok")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "test".into(),
            instructions: "test".into(),
            max_turns: 5,
            max_output_tokens: 8_000,
            ..RunnerConfig::default()
        },
    )
}

/// Removing one duplicate targets its identity and leaves the other copy.
#[test]
fn test_remove_exact_id() {
    let r = bare_runner();
    let first = QueuedInput::new("same");
    let second = QueuedInput::new("same");
    r.enqueue_input(first.clone());
    r.enqueue_input(second.clone());
    r.remove_input(second.id);
    assert_eq!(r.queued_input_snapshot(), vec![first.text]);
}
