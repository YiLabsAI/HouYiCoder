use super::*;
use houyicoder_resilience::resource_breaker::{ResourceBreaker, ResourceBreakerConfig, SpawnEvent};

fn usage(input: u32, output: u32, cache_read: u32) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        non_cached_input_tokens: input - cache_read,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    }
}

#[test]
fn test_accumulator_sums_seven_fields() {
    let mut acc = UsageAccumulator::default();
    acc.record(&usage(100, 50, 80));
    acc.record(&usage(200, 30, 120));
    let c = acc.cumulative();
    assert_eq!(c.input_tokens, 300);
    assert_eq!(c.output_tokens, 80);
    assert_eq!(c.total_tokens, 380);
    assert_eq!(c.cache_read_input_tokens, 200);
    assert_eq!(c.non_cached_input_tokens, 100); // (100-80)+(200-120)
    // last_input tracks the final response, not the sum.
    assert_eq!(acc.last_input_tokens(), 200);
}

#[test]
fn test_accumulator_reset_clears_tally() {
    let mut acc = UsageAccumulator::default();
    acc.record(&usage(100, 50, 0));
    assert_eq!(acc.cumulative().input_tokens, 100);
    acc.reset();
    assert_eq!(acc.cumulative(), Usage::default());
    assert_eq!(acc.last_input_tokens(), 0);
}

#[test]
fn test_record_tool_counts() {
    // ok = returned a value (not an error payload). The /context tool
    // tally splits calls into success vs error so the user sees failures.
    let mut acc = UsageAccumulator::default();
    acc.record_tool(true);
    acc.record_tool(true);
    acc.record_tool(false);
    assert_eq!(acc.tool_calls(), 3);
    assert_eq!(acc.tool_success(), 2);
    assert_eq!(acc.tool_errors(), 1);
    // reset clears tool tallies too (a new session starts at zero).
    acc.reset();
    assert_eq!(acc.tool_calls(), 0);
    assert_eq!(acc.tool_success(), 0);
    assert_eq!(acc.tool_errors(), 0);
}

#[test]
fn test_record_tool_batch_sums() {
    // resolve_turn counts a partition batch then records under one lock;
    // successive batches accumulate across turns.
    let mut acc = UsageAccumulator::default();
    acc.record_tool_batch(10, 7, 3);
    acc.record_tool_batch(5, 5, 0);
    assert_eq!(acc.tool_calls(), 15);
    assert_eq!(acc.tool_success(), 12);
    assert_eq!(acc.tool_errors(), 3);
}

/// redundancy_snapshot returns the tracker's flagged calls (empty for a
/// fresh runner with no redundant calls flagged). Pins the /trajectory
/// redundant-section data source.
#[test]
fn test_redundancy_snapshot_empty() {
    let runner = crate::agent::Runner::with_shared_store(
        std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        ))),
        std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("x")),
        crate::agent::ToolRegistry::new(),
        crate::agent::RunnerConfig {
            model: "stub-model".into(),
            ..crate::agent::RunnerConfig::default()
        },
    );
    assert!(
        runner.redundancy_snapshot().is_empty(),
        "fresh runner flags no redundant calls"
    );
}

#[test]
fn test_snapshot_reports_breaker() {
    // A breaker that trips on a single oversized End.
    let breaker = Arc::new(ResourceBreaker::new(ResourceBreakerConfig {
        aggregate_cpu_budget_secs: 10,
        ..ResourceBreakerConfig::default()
    }));
    breaker.record(SpawnEvent::Start { proc_count: 1 });
    breaker.record(SpawnEvent::End {
        cpu_secs: 11,
        proc_count: 1,
        exceeded_budget: false,
    });
    // Build a runner carrying the breaker + a primed accumulator.
    let runner = crate::agent::Runner::with_shared_store(
        std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        ))),
        std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("x")),
        crate::agent::ToolRegistry::new(),
        crate::agent::RunnerConfig {
            model: "stub-model".into(),
            ..crate::agent::RunnerConfig::default()
        },
    )
    .with_breaker(breaker);
    // Prime the accumulator as the drive loop would.
    runner
        .usage
        .lock()
        .unwrap()
        .record(&usage(12_400, 9_100, 10_000));
    let snap = runner.status_snapshot();
    assert_eq!(snap.model, "stub-model");
    // The breaker state is surfaced as a render label, not the resilience
    // enum — the host never names the type. Trip reason arrives as a
    // pre-rendered string carrying the AggregateCpuExceeded figures.
    assert_eq!(snap.breaker_state, Some("Open"));
    assert!(matches!(
        snap.breaker_reason.as_deref(),
        Some(s) if s.contains("AggregateCpuExceeded") && s.contains("11s") && s.contains("10s")
    ));
    // Open with cool-down unelapsed: a remaining duration is surfaced.
    assert!(snap.breaker_cool_down.is_some());
    assert_eq!(snap.cumulative_usage.input_tokens, 12_400);
    assert_eq!(snap.last_input_tokens, 12_400);
    assert_eq!(snap.context_window, 200_000); // FakeProvider default caps.
}

#[test]
fn test_snapshot_omits_absent_breaker() {
    let runner = crate::agent::Runner::with_shared_store(
        std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        ))),
        std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("x")),
        crate::agent::ToolRegistry::new(),
        crate::agent::RunnerConfig::default(),
    );
    let snap = runner.status_snapshot();
    assert!(snap.breaker_state.is_none());
    assert!(snap.breaker_reason.is_none());
    assert_eq!(snap.last_input_tokens, 0);
}

#[test]
fn test_snapshot_reflects_model_switch() {
    // After set_model, status_snapshot must read the live active_model and
    // the window that model resolves to, not the static config.model and
    // provider-caps values a pre-switch read would see. A switch to a
    // catalog model with a different window must surface both fields.
    let runner = crate::agent::Runner::with_shared_store(
        std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        ))),
        std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("x")),
        crate::agent::ToolRegistry::new(),
        crate::agent::RunnerConfig {
            model: "stub-model".into(),
            ..crate::agent::RunnerConfig::default()
        },
    );
    let before = runner.status_snapshot();
    assert_eq!(before.model, "stub-model");
    assert_eq!(
        before.context_window, 200_000,
        "no-signal model trusts provider caps"
    );
    runner.set_model("glm-5.2".into());
    let after = runner.status_snapshot();
    assert_eq!(
        after.model, "glm-5.2",
        "snapshot reads active_model, not config.model"
    );
    // FakeProvider reports 200K (non-zero), so the provider's window wins
    // over the catalog (which would give glm-5.2 1M). In production,
    // OpenAiCompatibleProvider reports 0 (unknown), so the catalog is
    // consulted. The priority: provider non-zero > catalog.
    assert_eq!(
        after.context_window, 200_000,
        "provider non-zero wins over catalog"
    );
}

/// active_effort starts None (follow the resolution chain) and set_effort
/// swaps it sticky for the session. The next request carries the new
/// level; None reverts to auto.
#[test]
fn test_set_effort_swaps_pick() {
    use houyicoder_protocol::llm::EffortLevel;
    let runner = crate::agent::Runner::with_shared_store(
        std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        ))),
        std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("x")),
        crate::agent::ToolRegistry::new(),
        crate::agent::RunnerConfig::default(),
    );
    assert!(
        runner.active_effort().is_none(),
        "fresh runner follows the resolution chain"
    );
    runner.set_effort(Some(EffortLevel::High));
    assert_eq!(runner.active_effort(), Some(EffortLevel::High));
    runner.set_effort(None);
    assert!(
        runner.active_effort().is_none(),
        "None reverts to the resolution chain"
    );
}

/// hooks_list returns empty when no hook registry is wired (the
/// common case for tests + stub runners).
#[test]
fn test_hooks_list_without_registry() {
    let runner = crate::agent::Runner::with_shared_store(
        std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        ))),
        std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("x")),
        crate::agent::ToolRegistry::new(),
        crate::agent::RunnerConfig::default(),
    );
    assert!(runner.hooks_list().is_empty());
}

/// skills_snapshot returns empty when no skill registry is wired (the
/// common case for tests + stub runners), so the /skills pane renders
/// an empty list rather than panicking on the None.
#[test]
fn test_skills_snapshot_without_registry() {
    let runner = crate::agent::Runner::with_shared_store(
        std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        ))),
        std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("x")),
        crate::agent::ToolRegistry::new(),
        crate::agent::RunnerConfig::default(),
    );
    assert!(runner.skills_snapshot().is_empty());
}

/// skills_snapshot returns the registry's model-invocable list, so the
/// /skills pane surfaces the same descriptors the turn-entry listing
/// step attaches. Order + fields pass through unchanged.
#[test]
fn test_skills_snapshot_lists_registry() {
    struct SnapshotStubRegistry;
    impl houyicoder_api::skill::SkillRegistry for SnapshotStubRegistry {
        fn list_model_invocable(&self) -> Vec<houyicoder_api::skill::SkillDescriptor> {
            Vec::new()
        }
        fn list_with_origin(&self) -> Vec<houyicoder_api::skill::SkillSnapshot> {
            vec![
                houyicoder_api::skill::SkillSnapshot {
                    descriptor: houyicoder_api::skill::SkillDescriptor {
                        name: "pdf-export".into(),
                        description: "export chat to pdf".into(),
                        when_to_use: None,
                        argument_hint: None,
                        disable_model_invocation: false,
                        user_invocable: true,
                        body_token_estimate: 320,
                        allowed_tools: Vec::new(),
                    },
                    origin: "user".into(),
                },
                houyicoder_api::skill::SkillSnapshot {
                    descriptor: houyicoder_api::skill::SkillDescriptor {
                        name: "internal-only".into(),
                        description: "host-restricted".into(),
                        when_to_use: None,
                        argument_hint: None,
                        disable_model_invocation: true,
                        user_invocable: false,
                        body_token_estimate: 80,
                        allowed_tools: Vec::new(),
                    },
                    origin: "project".into(),
                },
            ]
        }
        fn find(&self, _: &str) -> Option<houyicoder_api::skill::SkillDescriptor> {
            None
        }
        fn prepare_body(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<String, houyicoder_api::skill::SkillError> {
            Err(houyicoder_api::skill::SkillError::NotFound(String::new()))
        }
    }
    let runner = crate::agent::Runner::with_shared_store(
        std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        ))),
        std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("x")),
        crate::agent::ToolRegistry::new(),
        crate::agent::RunnerConfig::default(),
    )
    .with_skill_registry(std::sync::Arc::new(SnapshotStubRegistry));
    let snap = runner.skills_snapshot();
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0].descriptor.name, "pdf-export");
    assert_eq!(snap[0].descriptor.body_token_estimate, 320);
    assert_eq!(snap[0].origin, "user");
    assert_eq!(snap[1].descriptor.name, "internal-only");
    assert!(snap[1].descriptor.disable_model_invocation);
    assert_eq!(snap[1].origin, "project");
}

/// memory_forget maps the wire scope label to a MemoryScope and routes the
/// delete via delete_memory_in_scope (so a /memory pane d-action on a
/// project row hits the project root, not just the auto copy). A recording
/// mock captures the scope arg so the label-to-enum mapping + the dispatch
/// are both covered.
#[test]
fn test_memory_forget_routes_scope() {
    use houyicoder_api::memory::MemoryProvider;
    use houyicoder_context::{MemoryEntry, MemoryError, MemoryScope, MemorySource};
    use std::collections::HashSet;

    struct RecordingMemory {
        deletes: Arc<Mutex<Vec<(String, MemoryScope)>>>,
    }
    impl MemoryProvider for RecordingMemory {
        fn recall(&self, _: &str, _: usize, _: &HashSet<String>) -> Vec<MemoryEntry> {
            Vec::new()
        }
        fn add(&self, _: MemoryEntry) -> Result<(), MemoryError> {
            Ok(())
        }
        fn delete_memory_in_scope(&self, key: &str, scope: MemoryScope) -> Result<(), MemoryError> {
            self.deletes.lock().unwrap().push((key.to_string(), scope));
            Ok(())
        }
    }
    let deletes = Arc::new(Mutex::new(Vec::new()));
    let provider = std::sync::Arc::new(RecordingMemory {
        deletes: deletes.clone(),
    });
    // Exercise the required trait methods so the mock has no dead code.
    drop(provider.recall("", 0, &HashSet::new()));
    drop(provider.add(MemoryEntry::new("k", "c", MemorySource::Project)));
    let runner = crate::agent::Runner::with_shared_store(
        std::sync::Arc::new(houyicoder_session::SessionStore::new(Box::new(
            houyicoder_memory::InMemoryBackend::new(),
        ))),
        std::sync::Arc::new(crate::provider::test_support::FakeProvider::text("x")),
        crate::agent::ToolRegistry::new(),
        crate::agent::RunnerConfig::default(),
    )
    .with_memory(provider);
    runner.memory_forget("k", "project").unwrap();
    let captured = deletes.lock().unwrap().clone();
    assert_eq!(captured.len(), 1, "delete routed once");
    assert_eq!(captured[0].0, "k", "key forwarded");
    assert_eq!(
        captured[0].1,
        MemoryScope::Project,
        "project label mapped to Project scope"
    );
    // An unknown label falls back to Auto (the single-root behavior) so a
    // bad client label never panics the dispatch path.
    runner.memory_forget("k2", "nonsense").unwrap();
    assert_eq!(
        deletes.lock().unwrap().last().unwrap().1,
        MemoryScope::Auto,
        "unknown label falls back to Auto"
    );
}
