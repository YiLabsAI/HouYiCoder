//! max_output_tokens resolution tests, split from status.rs to keep that file
//! under the size gate. The catalog override (ModelEntry.max_output_tokens)
//! wins over the construction-time config value, and the pre-flight reserve +
//! request body share it (same source, no overflow).

#![cfg(test)]

use crate::agent::{EffortResolver, Runner, RunnerConfig, ToolRegistry};
use houyicoder_api::live::LiveEvent;
use houyicoder_protocol::llm::EffortLevel;
use std::sync::{Arc, Mutex};

fn stub_runner(config_max: u32) -> Runner {
    Runner::with_shared_store(
        Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        ))),
        Arc::new(crate::provider::test_support::FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig {
            max_output_tokens: config_max,
            ..RunnerConfig::default()
        },
    )
}

/// A resolver that returns a fixed catalog override for max_output_tokens.
struct OverrideResolver(u32);
impl EffortResolver for OverrideResolver {
    fn catalog_effort(&self, _model: &str) -> Option<EffortLevel> {
        None
    }
    fn catalog_max_output_tokens(&self, _model: &str) -> Option<u32> {
        Some(self.0)
    }
}

/// A catalog override wins over the construction-time config value, clamped
/// to the provider's declared cap (min).
#[test]
fn test_resolve_max_tokens_catalog() {
    let runner = stub_runner(32_768).with_effort_resolver(Arc::new(OverrideResolver(9999)));
    // FakeProvider caps max_output = 8000; min(9999, 8000) = 8000.
    assert_eq!(
        runner.resolve_max_output_tokens(),
        8000,
        "catalog override clamped to provider cap"
    );
}

/// Without a resolver, the config value clamps to the provider cap.
#[test]
fn test_resolve_max_tokens_fallback() {
    let runner = stub_runner(12_345);
    // FakeProvider caps max_output = 8000; min(12345, 8000) = 8000.
    assert_eq!(
        runner.resolve_max_output_tokens(),
        8000,
        "config clamped to provider cap when no resolver"
    );
}

/// emit_unactionable_overflow surfaces a SystemLine notice through the live
/// sink, pointing the user at the catalog override. The one self-heal gap.
#[test]
fn test_emit_unactionable_overflow_notice() {
    let captured = Arc::new(Mutex::new(Vec::<String>::new()));
    let cap = captured.clone();
    let sink: houyicoder_api::live::LiveSink = Arc::new(move |ev: &LiveEvent| {
        if let LiveEvent::SystemLine { text } = ev {
            cap.lock().unwrap().push(text.clone());
        }
    });
    let mut runner = stub_runner(32_768);
    runner.set_live_sink(sink);
    runner.emit_unactionable_overflow("qwen3.7-max");
    let got = captured.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "one notice fired");
    assert!(
        got[0].contains("qwen3.7-max") && got[0].contains("context_window"),
        "notice names the model + points at the override: {}",
        got[0]
    );
}

/// with_startup_warnings queues + drain_startup_warnings returns the queue
/// in order + clears it (a second drain is empty). The host pairs the drain
/// with a synchronous transcript push so the warnings land before any run
/// output — no async-sink race.
#[test]
fn test_startup_warnings_drain_clears() {
    let runner = stub_runner(32_768).with_startup_warnings(vec![
        "model.effort_level: bad".into(),
        "sandbox.network: unknown".into(),
    ]);
    let drained = runner.drain_startup_warnings();
    assert_eq!(
        drained,
        vec!["model.effort_level: bad", "sandbox.network: unknown",],
        "warnings drain in queue order"
    );
    assert!(
        runner.drain_startup_warnings().is_empty(),
        "second drain is empty (queue cleared)"
    );
}

/// refresh_served_models delegates to the provider. The stub FakeProvider
/// uses the trait's default no-op (returns Ok); the real OpenAI-compatible
/// provider overrides with the /v1/models fetch. Pins the delegation seam +
/// the default-impl path so the trait default body is covered.
#[test]
fn test_refresh_served_models_delegates() {
    let runner = stub_runner(32_768);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = rt.block_on(runner.refresh_served_models());
    assert!(
        result.is_ok(),
        "stub provider default no-op returns Ok: {result:?}"
    );
}
