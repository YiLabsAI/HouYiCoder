//! Tests for the hook system: verdicts, events, payloads, registry dispatch,
//! and deny-wins arbitration.

use super::registry::HookRegistry;
use super::*;
use houyicoder_context::{CheckpointId, SessionId};
use std::path::PathBuf;

use super::ToolResult;
use crate::agent::step::AgentId;

// --- test doubles ---

/// A hook that returns a fixed verdict for any subscribed event.
struct FixedHook {
    name: String,
    events: Vec<HookEvent>,
    verdict: HookVerdict,
    source: HookSource,
}

impl Hook for FixedHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn events(&self) -> &[HookEvent] {
        &self.events
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        Ok(self.verdict.clone())
    }
    fn source(&self) -> HookSource {
        self.source.clone()
    }
}

/// A hook that always errors.
struct ErrorHook {
    name: String,
    events: Vec<HookEvent>,
    error: HookError,
    source: HookSource,
}

impl Hook for ErrorHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn events(&self) -> &[HookEvent] {
        &self.events
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        Err(self.error.clone())
    }
    fn source(&self) -> HookSource {
        self.source.clone()
    }
}

/// A hook that sleeps before returning its verdict, for timeout tests.
struct SlowHook {
    name: String,
    events: Vec<HookEvent>,
    sleep_ms: u64,
    verdict: HookVerdict,
    source: HookSource,
}

impl Hook for SlowHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn events(&self) -> &[HookEvent] {
        &self.events
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        std::thread::sleep(std::time::Duration::from_millis(self.sleep_ms));
        Ok(self.verdict.clone())
    }
    fn source(&self) -> HookSource {
        self.source.clone()
    }
}

/// A hook that panics, for isolation tests.
struct PanicHook {
    name: String,
    events: Vec<HookEvent>,
    source: HookSource,
}

impl Hook for PanicHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn events(&self) -> &[HookEvent] {
        &self.events
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        panic!("boom");
    }
    fn source(&self) -> HookSource {
        self.source.clone()
    }
}

fn session_ctx(event: HookEvent, payload: HookPayload) -> HookContext {
    HookContext {
        event,
        payload,
        session: SessionId::new(),
    }
}

fn make_hook(events: Vec<HookEvent>, verdict: HookVerdict) -> Arc<FixedHook> {
    Arc::new(FixedHook {
        name: "fixed".into(),
        events,
        verdict,
        source: HookSource::User,
    })
}

fn make_sourced_hook(
    events: Vec<HookEvent>,
    verdict: HookVerdict,
    source: HookSource,
) -> Arc<FixedHook> {
    Arc::new(FixedHook {
        name: "fixed".into(),
        events,
        verdict,
        source,
    })
}

// ========================================================================
// HookVerdict: every variant constructs and matches.
// ========================================================================

#[test]
fn test_verdict_constructs_and_matches() {
    let cases = vec![
        HookVerdict::Allow,
        HookVerdict::Deny("no".into()),
        HookVerdict::Feedback("rewrite".into()),
        HookVerdict::Observe("noted".into()),
        HookVerdict::Inject("extra".into()),
        HookVerdict::Ask("confirm?".into()),
        HookVerdict::Trigger(HookEvent::PreCompact),
    ];
    for v in &cases {
        match v {
            HookVerdict::Allow => {}
            HookVerdict::Deny(r) => assert_eq!(r, "no"),
            HookVerdict::Feedback(s) => assert_eq!(s, "rewrite"),
            HookVerdict::Observe(n) => assert_eq!(n, "noted"),
            HookVerdict::Inject(c) => assert_eq!(c, "extra"),
            HookVerdict::Ask(q) => assert_eq!(q, "confirm?"),
            HookVerdict::Trigger(e) => assert_eq!(*e, HookEvent::PreCompact),
        }
    }
    assert_eq!(cases.len(), 7);
}

// ========================================================================
// HookEvent: exhaustive variant coverage (27 reference events + PreSelect).
// ========================================================================

#[test]
fn test_event_exhaustive_match() {
    let all = vec![
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::PostToolUseFailure,
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::Setup,
        HookEvent::UserPromptSubmit,
        HookEvent::Stop,
        HookEvent::StopFailure,
        HookEvent::Notification,
        HookEvent::PreCompact,
        HookEvent::PostCompact,
        HookEvent::PreSelect,
        HookEvent::InstructionsLoaded,
        HookEvent::CwdChanged,
        HookEvent::FileChanged,
        HookEvent::ConfigChange,
        HookEvent::SubagentStart,
        HookEvent::SubagentStop,
        HookEvent::PermissionRequest,
        HookEvent::PermissionDenied,
        HookEvent::TeammateIdle,
        HookEvent::TaskCreated,
        HookEvent::TaskCompleted,
        HookEvent::Elicitation,
        HookEvent::ElicitationResult,
        HookEvent::WorktreeCreate,
        HookEvent::WorktreeRemove,
    ];
    // 27 reference events + 1 select-phase event = 28.
    assert_eq!(all.len(), 28);
    // Exhaustive match: proves every variant is named and distinct.
    for ev in &all {
        let _ = match ev {
            HookEvent::PreToolUse
            | HookEvent::PostToolUse
            | HookEvent::PostToolUseFailure
            | HookEvent::SessionStart
            | HookEvent::SessionEnd
            | HookEvent::Setup
            | HookEvent::UserPromptSubmit
            | HookEvent::Stop
            | HookEvent::StopFailure
            | HookEvent::Notification
            | HookEvent::PreCompact
            | HookEvent::PostCompact
            | HookEvent::PreSelect
            | HookEvent::InstructionsLoaded
            | HookEvent::CwdChanged
            | HookEvent::FileChanged
            | HookEvent::ConfigChange
            | HookEvent::SubagentStart
            | HookEvent::SubagentStop
            | HookEvent::PermissionRequest
            | HookEvent::PermissionDenied
            | HookEvent::TeammateIdle
            | HookEvent::TaskCreated
            | HookEvent::TaskCompleted
            | HookEvent::Elicitation
            | HookEvent::ElicitationResult
            | HookEvent::WorktreeCreate
            | HookEvent::WorktreeRemove => 1,
        };
    }
    // Uniqueness: no two variants compare equal.
    for i in 0..all.len() {
        for j in (i + 1)..all.len() {
            assert_ne!(all[i], all[j], "dup at {i},{j}");
        }
    }
}

// ========================================================================
// HookPayload: every variant constructs and matches.
// ========================================================================

#[test]
#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
fn test_payload_exhaustive_construct() {
    let val = serde_json::json!({"k": 1});
    let s = "x".to_string();
    let tr = ToolResult {
        output: "ok".into(),
    };
    let ck = CheckpointId::new();
    let aid = AgentId("a1".into());
    let tid = TaskId("t1".into());
    let keys = vec!["k".to_string()];
    let paths = vec![PathBuf::from("/tmp")];
    let cwd = std::path::PathBuf::from("/cwd");

    let p = HookPayload::PreToolUse {
        tool_name: s.clone(),
        input: val.clone(),
        backfilled_input: Some(val.clone()),
    };
    assert!(matches!(p, HookPayload::PreToolUse { .. }));

    let p = HookPayload::PostToolUse {
        tool_name: s.clone(),
        input: val.clone(),
        result: tr.clone(),
    };
    assert!(matches!(p, HookPayload::PostToolUse { .. }));

    let p = HookPayload::PostToolUseFailure {
        tool_name: s.clone(),
        error: s.clone(),
    };
    assert!(matches!(p, HookPayload::PostToolUseFailure { .. }));

    let p = HookPayload::PreCompact {
        trigger: super::CompactTrigger::Manual,
        pre_compact_event_count: 10,
        pre_compact_token_estimate: 500,
    };
    assert!(matches!(p, HookPayload::PreCompact { .. }));

    let p = HookPayload::PostCompact {
        trigger: super::CompactTrigger::Auto,
        checkpoint_id: ck,
        folded_turns: 2,
        compression_ratio: 0.5,
        compact_summary: "summary text".into(),
    };
    assert!(matches!(p, HookPayload::PostCompact { .. }));

    let p = HookPayload::PreSelect {
        current_token_estimate: 100,
    };
    assert!(matches!(p, HookPayload::PreSelect { .. }));

    let p = HookPayload::SessionStart { resumed: false };
    assert!(matches!(p, HookPayload::SessionStart { .. }));

    let p = HookPayload::SessionEnd {
        reason: SessionEndReason::Clear,
    };
    assert!(matches!(p, HookPayload::SessionEnd { .. }));

    let p = HookPayload::UserPromptSubmit { prompt: s.clone() };
    assert!(matches!(p, HookPayload::UserPromptSubmit { .. }));

    let p = HookPayload::Stop { turn_count: 3 };
    assert!(matches!(p, HookPayload::Stop { .. }));

    let p = HookPayload::StopFailure { error: s.clone() };
    assert!(matches!(p, HookPayload::StopFailure { .. }));

    let p = HookPayload::Notification { message: s.clone() };
    assert!(matches!(p, HookPayload::Notification { .. }));

    let p = HookPayload::SubagentStart {
        agent_id: aid.clone(),
        agent_type: "coder".into(),
    };
    assert!(matches!(p, HookPayload::SubagentStart { .. }));

    let p = HookPayload::SubagentStop {
        agent_id: aid.clone(),
        agent_type: "coder".into(),
        status: "completed".into(),
        last_text: Some(s.clone()),
    };
    assert!(matches!(p, HookPayload::SubagentStop { .. }));

    let p = HookPayload::PermissionRequest {
        tool_name: s.clone(),
        action: s.clone(),
        resource: s.clone(),
    };
    assert!(matches!(p, HookPayload::PermissionRequest { .. }));

    let p = HookPayload::PermissionDenied {
        tool_name: s.clone(),
        reason: s.clone(),
    };
    assert!(matches!(p, HookPayload::PermissionDenied { .. }));

    let p = HookPayload::ConfigChange { changed_keys: keys };
    assert!(matches!(p, HookPayload::ConfigChange { .. }));

    let p = HookPayload::FileChanged { paths };
    assert!(matches!(p, HookPayload::FileChanged { .. }));

    let p = HookPayload::CwdChanged {
        new_cwd: cwd.clone(),
    };
    assert!(matches!(p, HookPayload::CwdChanged { .. }));

    let p = HookPayload::InstructionsLoaded { source: s.clone() };
    assert!(matches!(p, HookPayload::InstructionsLoaded { .. }));

    let p = HookPayload::WorktreeCreate { path: cwd.clone() };
    assert!(matches!(p, HookPayload::WorktreeCreate { .. }));

    let p = HookPayload::WorktreeRemove { path: cwd };
    assert!(matches!(p, HookPayload::WorktreeRemove { .. }));

    let p = HookPayload::Elicitation {
        request: val.clone(),
    };
    assert!(matches!(p, HookPayload::Elicitation { .. }));

    let p = HookPayload::ElicitationResult { result: val };
    assert!(matches!(p, HookPayload::ElicitationResult { .. }));

    let p = HookPayload::TaskCreated {
        task_id: tid.clone(),
    };
    assert!(matches!(p, HookPayload::TaskCreated { .. }));

    let p = HookPayload::TaskCompleted { task_id: tid };
    assert!(matches!(p, HookPayload::TaskCompleted { .. }));

    let p = HookPayload::TeammateIdle { agent_id: aid };
    assert!(matches!(p, HookPayload::TeammateIdle { .. }));

    let p = HookPayload::Setup;
    assert!(matches!(p, HookPayload::Setup));
}

// ========================================================================
// HookRegistry: register + dispatch returns the registered verdict.
// ========================================================================

#[test]
fn test_registry_dispatch_returns_verdict() {
    let hook = make_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Deny("blocked".into()),
    );
    let mut reg = HookRegistry::new();
    reg.register(hook);
    assert_eq!(reg.len(), 1);

    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    assert_eq!(results.len(), 1);
    match &results[0] {
        Ok(HookVerdict::Deny(r)) => assert_eq!(r, "blocked"),
        other => panic!("expected Deny, got {other:?}"),
    }
}

#[test]
fn test_registry_dispatch_no_subscribers() {
    let reg = HookRegistry::new();
    let ctx = session_ctx(HookEvent::PostToolUse, HookPayload::Setup);
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    assert!(results.is_empty());
}

#[test]
fn test_registry_disabled_policy_skips() {
    let hook = make_hook(vec![HookEvent::PreToolUse], HookVerdict::Deny("x".into()));
    let mut reg = HookRegistry::with_policy(HookPolicy::Disabled);
    reg.register(hook);
    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    assert!(reg.dispatch(&ctx).is_empty());
}

#[test]
fn test_registry_skips_unsubscribed() {
    let hook = make_hook(vec![HookEvent::SessionStart], HookVerdict::Allow);
    let mut reg = HookRegistry::new();
    reg.register(hook);
    // PreToolUse has no subscribers; SessionStart does.
    let ctx_pre = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    assert!(reg.dispatch(&ctx_pre).is_empty());
    let ctx_start = session_ctx(HookEvent::SessionStart, HookPayload::Setup);
    assert_eq!(reg.dispatch(&ctx_start).len(), 1);
}

// ========================================================================
// Policy filter: ManagedOnly / PluginOnly actually filter by source.
// ========================================================================

#[test]
fn test_policy_managed_only_filters() {
    let user_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Deny("user".into()),
        HookSource::User,
    );
    let managed_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Deny("managed".into()),
        HookSource::Managed,
    );
    let project_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Deny("project".into()),
        HookSource::Project,
    );
    let mut reg = HookRegistry::with_policy(HookPolicy::ManagedOnly);
    reg.register(user_hook);
    reg.register(managed_hook);
    reg.register(project_hook);
    assert_eq!(reg.len(), 3);

    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    // Only the managed-source hook passes the ManagedOnly filter.
    assert_eq!(results.len(), 1, "ManagedOnly must keep only Managed hooks");
    match &results[0] {
        Ok(HookVerdict::Deny(r)) => assert_eq!(r, "managed"),
        other => panic!("expected managed deny, got {other:?}"),
    }
}

#[test]
fn test_policy_plugin_only_filters() {
    let user_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Deny("user".into()),
        HookSource::User,
    );
    let project_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Deny("project".into()),
        HookSource::Project,
    );
    let mut reg = HookRegistry::with_policy(HookPolicy::PluginOnly);
    reg.register(user_hook);
    reg.register(project_hook);
    assert_eq!(reg.len(), 2);

    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    // PluginOnly keeps only Project-source hooks.
    assert_eq!(results.len(), 1, "PluginOnly must keep only Project hooks");
    match &results[0] {
        Ok(HookVerdict::Deny(r)) => assert_eq!(r, "project"),
        other => panic!("expected project deny, got {other:?}"),
    }
}

#[test]
fn test_policy_all_enabled_runs() {
    let user_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Allow,
        HookSource::User,
    );
    let managed_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Allow,
        HookSource::Managed,
    );
    let project_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Allow,
        HookSource::Project,
    );
    let local_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Allow,
        HookSource::Local,
    );
    let mut reg = HookRegistry::with_policy(HookPolicy::AllEnabled);
    reg.register(user_hook);
    reg.register(managed_hook);
    reg.register(project_hook);
    reg.register(local_hook);
    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    assert_eq!(reg.dispatch(&ctx).len(), 4, "AllEnabled runs every source");
}

// ========================================================================
// Trust filter: untrusted project skips Project + Local source hooks.
// ========================================================================

#[test]
fn test_trust_untrusted_skips_project() {
    let user_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Allow,
        HookSource::User,
    );
    let project_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Allow,
        HookSource::Project,
    );
    let managed_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Allow,
        HookSource::Managed,
    );
    let local_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Allow,
        HookSource::Local,
    );
    let mut reg =
        HookRegistry::with_policy_and_trust(HookPolicy::AllEnabled, TrustState::Untrusted);
    reg.register(user_hook);
    reg.register(project_hook);
    reg.register(managed_hook);
    reg.register(local_hook);
    assert_eq!(reg.len(), 4);

    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    // Untrusted skips Project AND Local (both live in the repo dir).
    // User and Managed pass.
    assert_eq!(
        results.len(),
        2,
        "untrusted must skip Project and Local sources"
    );
}

#[test]
fn test_trust_untrusted_skips_local() {
    let local_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Deny("local".into()),
        HookSource::Local,
    );
    let mut reg =
        HookRegistry::with_policy_and_trust(HookPolicy::AllEnabled, TrustState::Untrusted);
    reg.register(local_hook);
    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    assert!(
        results.is_empty(),
        "untrusted project must skip Local source hooks"
    );
    // The skip notice is queued for the caller to drain as a system line.
    let skipped = reg.take_skipped_untrusted();
    assert!(skipped.is_some(), "untrusted skip queues a one-time notice");
    let names = skipped.unwrap();
    assert!(
        !names.is_empty(),
        "notice lists the skipped hook names: {names:?}"
    );
    // A second drain is empty — the notice fires once.
    assert!(
        reg.take_skipped_untrusted().is_none(),
        "notice fires once, not per-dispatch"
    );
}

#[test]
fn test_untrusted_skip_notice() {
    // The trust gate is scaffolded (Untrusted is never set in production
    // yet), but the drain path must still surface a queued skip notice as a
    // system line when a project hook is registered against an untrusted
    // registry. The notice must name the skipped hooks and must not point at
    // an escape hatch that does not exist.
    use crate::agent::ToolRegistry;
    use crate::agent::tests::runner_with;
    use houyicoder_api::live::{LiveEvent, LiveSink};
    use std::sync::{Arc, Mutex};

    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = Arc::clone(&captured);
    let mut reg =
        HookRegistry::with_policy_and_trust(HookPolicy::AllEnabled, TrustState::Untrusted);
    reg.register(make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Allow,
        HookSource::Project,
    ));
    let mut runner = runner_with(
        Arc::new(crate::provider::test_support::FakeProvider::text("ok")),
        ToolRegistry::new(),
    );
    runner.set_live_sink(Arc::new(move |ev: &LiveEvent| {
        if let LiveEvent::SystemLine { text } = ev {
            cap.lock().unwrap().push(text.clone());
        }
    }) as LiveSink);
    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    runner.dispatch_hooks(&reg, &ctx);
    let lines = captured.lock().unwrap();
    assert!(
        !lines.is_empty(),
        "the skip notice surfaces as a system line"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("untrusted project hooks skipped")),
        "notice names the skip: {lines:?}"
    );
    assert!(
        lines.iter().all(|l| !l.contains("/trust")),
        "notice must not name an escape hatch that does not exist: {lines:?}"
    );
}

#[test]
fn test_trust_acknowledged_runs_all() {
    let project_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Allow,
        HookSource::Project,
    );
    let local_hook = make_sourced_hook(
        vec![HookEvent::PreToolUse],
        HookVerdict::Allow,
        HookSource::Local,
    );
    let mut reg =
        HookRegistry::with_policy_and_trust(HookPolicy::AllEnabled, TrustState::Acknowledged);
    reg.register(project_hook);
    reg.register(local_hook);
    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    assert_eq!(
        reg.dispatch(&ctx).len(),
        2,
        "acknowledged runs project and local hooks"
    );
}

// ========================================================================
// Parallel dispatch + per-hook timeout + panic isolation.
// ========================================================================

#[test]
fn test_parallel_dispatch_returns_all() {
    let hook_a = make_hook(vec![HookEvent::PreToolUse], HookVerdict::Allow);
    let hook_b = make_hook(vec![HookEvent::PreToolUse], HookVerdict::Deny("b".into()));
    let hook_c = make_hook(vec![HookEvent::PreToolUse], HookVerdict::Allow);
    let mut reg = HookRegistry::new().with_timeout(5000);
    reg.register(hook_a);
    reg.register(hook_b);
    reg.register(hook_c);
    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    assert_eq!(results.len(), 3, "all hooks dispatched in parallel");
    // Results in registration order.
    assert!(results[0].is_ok());
    match &results[1] {
        Ok(HookVerdict::Deny(r)) => assert_eq!(r, "b"),
        other => panic!("expected Deny from hook_b, got {other:?}"),
    }
    assert!(results[2].is_ok());
}

#[test]
fn test_timeout_returns_timeout_error() {
    let slow_hook = Arc::new(SlowHook {
        name: "slow".into(),
        events: vec![HookEvent::PreToolUse],
        sleep_ms: 200,
        verdict: HookVerdict::Allow,
        source: HookSource::User,
    });
    // timeout_ms=20, hook sleeps 200ms: must time out.
    let mut reg = HookRegistry::new().with_timeout(20);
    reg.register(slow_hook);
    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    assert_eq!(results.len(), 1);
    match &results[0] {
        Err(HookError::Timeout {
            hook_name,
            limit_ms,
        }) => {
            assert_eq!(*hook_name, "slow");
            assert_eq!(*limit_ms, 20);
        }
        other => panic!("expected Timeout, got {other:?}"),
    }
}

#[test]
fn test_timeout_bounded_wall_clock() {
    // A hook that sleeps 2000ms with a 50ms timeout must NOT block dispatch
    // for 2000ms. The previous scoped-thread implementation blocked until
    // the thread finished, so dispatch took 2000ms. The detached-thread
    // implementation must return within a small epsilon of the timeout.
    let slow_hook = Arc::new(SlowHook {
        name: "very-slow".into(),
        events: vec![HookEvent::PreToolUse],
        sleep_ms: 2000,
        verdict: HookVerdict::Allow,
        source: HookSource::User,
    });
    let mut reg = HookRegistry::new().with_timeout(50);
    reg.register(slow_hook);
    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);

    let start = Instant::now();
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    let elapsed = start.elapsed();

    assert_eq!(results.len(), 1);
    match &results[0] {
        Err(HookError::Timeout { hook_name, .. }) => assert_eq!(hook_name, "very-slow"),
        other => panic!("expected Timeout, got {other:?}"),
    }
    // Dispatch must return well before the hook's 2000ms sleep.
    // Allow generous epsilon for thread spawn + scheduling overhead.
    assert!(
        elapsed < Duration::from_millis(500),
        "dispatch took {:?}, expected < 500ms (timeout=50ms, hook sleep=2000ms)",
        elapsed
    );
}

#[test]
fn test_panic_returns_guest_panic() {
    let panic_hook = Arc::new(PanicHook {
        name: "panic-hook".into(),
        events: vec![HookEvent::PreToolUse],
        source: HookSource::User,
    });
    let mut reg = HookRegistry::new().with_timeout(0);
    reg.register(panic_hook);
    let ctx = session_ctx(HookEvent::PreToolUse, HookPayload::Setup);
    let results: Vec<_> = reg.dispatch(&ctx).into_iter().map(|o| o.result).collect();
    assert_eq!(results.len(), 1);
    match &results[0] {
        Err(HookError::GuestPanic { hook_name, .. }) => {
            assert_eq!(hook_name, "panic-hook");
        }
        other => panic!("expected GuestPanic, got {other:?}"),
    }
}

#[path = "hook_arbitrate_tests.rs"]
mod arbitrate_tests;

// ========================================================================
// HookError: every variant constructs.
// ========================================================================

#[test]
fn test_error_variants_construct() {
    let errs = vec![
        HookError::GuestPanic {
            hook_name: "h".into(),
            backtrace: "bt".into(),
        },
        HookError::Timeout {
            hook_name: "h".into(),
            limit_ms: 5000,
        },
        HookError::InvalidVerdict {
            hook_name: "h".into(),
            detail: "bad".into(),
        },
        HookError::CapabilityDenied {
            hook_name: "h".into(),
            capability: "fs-read".into(),
        },
        HookError::FeedbackExhausted {
            hook_name: "h".into(),
            attempts: 3,
        },
        HookError::ConfigError {
            detail: "malformed".into(),
        },
        HookError::ProcessError {
            hook_name: "h".into(),
            reason: "dead".into(),
        },
    ];
    for e in &errs {
        let _ = match e {
            HookError::GuestPanic { .. }
            | HookError::Timeout { .. }
            | HookError::InvalidVerdict { .. }
            | HookError::CapabilityDenied { .. }
            | HookError::FeedbackExhausted { .. }
            | HookError::ConfigError { .. }
            | HookError::ProcessError { .. } => true,
        };
    }
    assert_eq!(errs.len(), 7);
}
