use super::*;
use houyicoder_context::{SessionEvent, SessionId};
use houyicoder_memory::InMemoryBackend;
use houyicoder_resilience::resource_breaker::{ResourceBreaker, ResourceBreakerConfig, SpawnEvent};
use houyicoder_session::SessionStore;

use crate::agent::Runner;
use crate::agent::ToolRegistry;
use crate::agent::append::new_event;
use crate::agent::runner_config::RunnerConfig;
use crate::provider::test_support::FakeProvider;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_api::skill::SkillDescriptor;
use houyicoder_api::skill::SkillError;
use houyicoder_api::skill::SkillRegistry;
use houyicoder_api::skill::SkillSnapshot;
use houyicoder_context::{MemoryEntry, MemoryError, MemoryScope, MemorySource};
use houyicoder_protocol::llm::EffortLevel;
use std::collections::HashSet;

fn usage(input: u32, output: u32, cache_read: u32) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        non_cached_input_tokens: input - cache_read,
        cache_read_input_tokens: cache_read,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        ..Default::default()
    }
}

/// A logged turn as the session reader hands it back: the token figures the
/// endpoint reported plus whether each was reported at all.
fn logged_turn(
    session: SessionId,
    input: u64,
    output: u64,
    cache_read: u64,
    reported: bool,
) -> SessionLogEntry {
    new_event(
        session,
        SessionEvent::TurnUsage {
            turn: 1,
            call_in_turn: 1,
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cache_read,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            model: "logged-model".into(),
            recovery: false,
            effort: None,
            input_reported: reported,
            cache_read_reported: reported,
            cache_write_reported: false,
        },
    )
}

/// A runner that reopened a session continues that session's spend: the turns
/// its log already holds are part of the tally, so the parent half covers the
/// same session the delegated half does. Before this, a reopened session
/// reported only the turns it had run itself.
#[test]
fn test_seed_folds_logged_turns() {
    let session = SessionId::new();
    let logged = vec![
        logged_turn(session, 9_000, 400, 7_200, true),
        logged_turn(session, 3_000, 100, 2_400, true),
    ];
    let mut acc = UsageAccumulator::default();
    acc.seed_from_events(&logged);
    let c = acc.cumulative();
    assert_eq!(c.input_tokens, 12_000, "the whole session's input");
    assert_eq!(c.output_tokens, 500);
    assert_eq!(c.cache_read_input_tokens, 9_600);
    assert_eq!(
        c.non_cached_input_tokens, 2_400,
        "the non-cached share of both turns"
    );
    assert_eq!(acc.last_input_tokens(), 3_000, "the newest logged turn");
    assert_eq!(
        c.cache_hit_rate(),
        Some(0.8),
        "every logged turn reported the parts the rate needs"
    );
}

/// A log written before the completeness flags existed reads as unreported, so
/// a seeded total leaves the cache account unknown rather than presenting the
/// recorded figures as a rate the log never carried.
#[test]
fn test_seed_unreported_turn_unknown() {
    let session = SessionId::new();
    let mut acc = UsageAccumulator::default();
    acc.seed_from_events(&[logged_turn(session, 1_000, 100, 800, false)]);
    let c = acc.cumulative();
    assert_eq!(c.input_tokens, 1_000, "the recorded figure still counts");
    assert_eq!(
        c.cache_hit_rate(),
        None,
        "an unreported turn leaves the rate unknown, not guessed"
    );
}

/// A fresh session's log holds no turn yet, so seedling it changes nothing:
/// the resumed path and the new-session path report the same empty tally.
#[test]
fn test_seed_log_without_usage() {
    let session = SessionId::new();
    let log = vec![
        new_event(
            session,
            SessionEvent::UserInput {
                text: "first".into(),
            },
        ),
        new_event(session, SessionEvent::RunCompleted { ms: Some(5) }),
    ];
    let mut acc = UsageAccumulator::default();
    acc.seed_from_events(&log);
    assert_eq!(acc.cumulative().input_tokens, 0);
    assert_eq!(acc.last_input_tokens(), 0);
    assert_eq!(acc.tool_calls(), 0, "the tool counters stay per-run");
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
    assert_eq!(acc.cumulative().input_tokens, 0, "reset clears the tally");
    assert_eq!(acc.last_input_tokens(), 0);
}

/// The cumulative cache account is complete only while every folded turn
/// reported input and cache-read; one unreported part turns the whole tally
/// unknown rather than a silently partial rate.
#[test]
fn test_accumulator_folds_provenance() {
    let complete = Usage {
        input_tokens: 1000,
        cache_read_input_tokens: 800,
        provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: true,
            cache_write_reported: false,
        },
        ..Default::default()
    };
    let unreported = Usage {
        input_tokens: 500,
        cache_read_input_tokens: 0,
        provenance: UsageProvenance {
            input_reported: true,
            cache_read_reported: false,
            cache_write_reported: false,
        },
        ..Default::default()
    };
    let mut acc = UsageAccumulator::default();
    acc.record(&complete);
    assert_eq!(acc.cumulative().cache_hit_rate(), Some(0.8));
    acc.record(&unreported);
    assert_eq!(
        acc.cumulative().cache_hit_rate(),
        None,
        "an unreported part makes the total rate unknown"
    );
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

/// A delegation tool result carries the child's usage, but it is written when
/// the tool returns: before a background child has run, and without a block
/// when an interrupted one gives up. Folding it into the tally therefore
/// understates the session, so the append path leaves the tally alone and the
/// durable return stays the one complete record.
#[tokio::test]
async fn test_delegation_result_not_folded() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let runner = Runner::new(
        store.clone(),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    let session = SessionId::new();
    store
        .append(new_event(
            session,
            SessionEvent::ToolCall {
                call_id: "c1".into(),
                tool: "agent".into(),
                input: serde_json::json!({}),
            },
        ))
        .await
        .unwrap();
    let output = serde_json::json!({
        "status": "completed",
        "usage": {
            "input_tokens": 500,
            "output_tokens": 40,
            "cache_read_input_tokens": 100,
        },
    });
    runner
        .append_tool_result(session, "c1".into(), "agent", output, 0)
        .await
        .unwrap();
    let snap = runner.status_snapshot();
    assert_eq!(
        snap.cumulative_usage.input_tokens, 0,
        "a delegation tool result must not move the session tally"
    );
    assert_eq!(snap.cumulative_usage.output_tokens, 0);
    assert_eq!(
        snap.last_input_tokens, 0,
        "and it must not claim the parent window either"
    );
}

/// redundancy_snapshot returns the tracker's flagged calls (empty for a
/// fresh runner with no redundant calls flagged). Pins the /trajectory
/// redundant-section data source.
#[test]
fn test_redundancy_snapshot_empty() {
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "stub-model".into(),
            ..RunnerConfig::default()
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
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "stub-model".into(),
            ..RunnerConfig::default()
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
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
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
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig {
            model: "stub-model".into(),
            ..RunnerConfig::default()
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
    // One chain for display and request: the family table (glm-5.2 => 1M)
    // outranks the provider's non-zero window, because a provider-declared
    // window is one source, not an override. FakeProvider reports 200K; the
    // catalog knows the model and wins.
    assert_eq!(
        after.context_window, 1_000_000,
        "catalog family table wins over provider window"
    );
}

/// active_effort starts None (follow the resolution chain) and set_effort
/// swaps it sticky for the session. The next request carries the new
/// level; None reverts to auto.
#[test]
fn test_set_effort_swaps_pick() {
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
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
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    assert!(runner.hooks_list().is_empty());
}

/// skills_snapshot returns empty when no skill registry is wired (the
/// common case for tests + stub runners), so the /skills pane renders
/// an empty list rather than panicking on the None.
#[test]
fn test_skills_snapshot_without_registry() {
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    assert!(runner.skills_snapshot().is_empty());
}

/// skills_snapshot returns the registry's model-invocable list, so the
/// /skills pane surfaces the same descriptors the turn-entry listing
/// step attaches. Order + fields pass through unchanged.
#[test]
fn test_skills_snapshot_lists_registry() {
    struct SnapshotStubRegistry;
    impl SkillRegistry for SnapshotStubRegistry {
        fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
            Vec::new()
        }
        fn list_with_origin(&self) -> Vec<SkillSnapshot> {
            vec![
                SkillSnapshot {
                    descriptor: SkillDescriptor {
                        name: "pdf-export".into(),
                        description: "export chat to pdf".into(),
                        when_to_use: None,
                        argument_hint: None,
                        disable_model_invocation: false,
                        user_invocable: true,
                        body_token_estimate: 320,
                        allowed_tools: Vec::new(),
                        allowed_mach_services: Vec::new(),
                        allow_app_launch: false,
                    },
                    origin: "user".into(),
                    usage: Default::default(),
                },
                SkillSnapshot {
                    descriptor: SkillDescriptor {
                        name: "internal-only".into(),
                        description: "host-restricted".into(),
                        when_to_use: None,
                        argument_hint: None,
                        disable_model_invocation: true,
                        user_invocable: false,
                        body_token_estimate: 80,
                        allowed_tools: Vec::new(),
                        allowed_mach_services: Vec::new(),
                        allow_app_launch: false,
                    },
                    origin: "project".into(),
                    usage: Default::default(),
                },
            ]
        }
        fn find(&self, _: &str) -> Option<SkillDescriptor> {
            None
        }
        fn prepare_body(
            &self,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
        ) -> Result<String, SkillError> {
            Err(SkillError::NotFound(String::new()))
        }
    }
    let runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
    .with_skill_registry(Arc::new(SnapshotStubRegistry));
    let snap = runner.skills_snapshot();
    assert_eq!(snap.len(), 2);
    assert_eq!(snap[0].descriptor.name, "pdf-export");
    assert_eq!(snap[0].descriptor.body_token_estimate, 320);
    assert_eq!(snap[0].origin, "user");
    assert_eq!(snap[1].descriptor.name, "internal-only");
    assert!(snap[1].descriptor.disable_model_invocation);
    assert_eq!(snap[1].origin, "project");
}

/// memory_forget maps the protocol scope label to a MemoryScope and routes the
/// delete via delete_memory_in_scope (so a /memory pane d-action on a
/// project row hits the project root, not just the auto copy). A recording
/// mock captures the scope arg so the label-to-enum mapping + the dispatch
/// are both covered.
#[test]
fn test_memory_forget_routes_scope() {
    struct RecordingMemory {
        deletes: Arc<Mutex<Vec<(String, MemoryScope)>>>,
    }
    impl MemoryProvider for RecordingMemory {
        fn add(&self, _: MemoryEntry) -> Result<(), MemoryError> {
            Ok(())
        }
        fn delete_memory_in_scope(&self, key: &str, scope: MemoryScope) -> Result<(), MemoryError> {
            self.deletes.lock().unwrap().push((key.to_string(), scope));
            Ok(())
        }
    }
    let deletes = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(RecordingMemory {
        deletes: deletes.clone(),
    });
    // Exercise the mock methods so the stub has no dead code.
    drop(provider.rank_candidates("", &HashSet::new()));
    drop(provider.add(MemoryEntry::new("k", "c", MemorySource::Project)));
    let mut runner = Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    );
    runner.memory.install_provider(provider);
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

mod status_override;
