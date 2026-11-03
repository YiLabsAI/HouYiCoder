//! Child spawn boundaries, cancellation, status, and inbox behavior.

use super::{RunnerInheritance, SpawnError, SpawnRequest, TriggerSource, spawn_child};
use houyicoder_api::provider::ModelProvider;
use houyicoder_api::skill::{SkillDescriptor, SkillError, SkillRegistry};
use houyicoder_async::CancellationToken;
use houyicoder_async::PFut;
use houyicoder_context::{
    EventId, PromptCoverage, SessionDescriptor, SessionDescriptorStore, SessionEvent, SessionId,
    SessionLogEntry, SessionProvenance,
};
use houyicoder_memory::{InMemoryBackend, InMemoryDescriptorStore};
use houyicoder_session::SessionStore;
use std::any::Any;
use std::sync::Arc;

use crate::agent::multi_agent::bus_types::ChildRunMode;
use crate::agent::multi_agent::bus_types::progress_topic;
use crate::agent::multi_agent::bus_types::{AgentBus, BusMessage, completed_topic};
use crate::agent::multi_agent::registry::IsolationMode;
use crate::agent::runner_config::RunnerConfig;
use crate::agent::{
    Hook, HookContext, HookError, HookEvent, HookRegistry, HookSource, HookVerdict, SummarizeError,
    Summarizer, ToolRegistry,
};
use crate::provider::test_support::FakeProvider;
use houyicoder_async::bus::MessageBus;
use houyicoder_protocol::frontend::completion::CompletionStatus;
use houyicoder_protocol::llm::EffortLevel;
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

fn req_at_depth(
    parent_sid: SessionId,
    store: Arc<SessionStore>,
    provider: Arc<dyn ModelProvider>,
    depth: u32,
) -> SpawnRequest {
    SpawnRequest {
        parent_sid,
        parent_store: store,
        provider,
        tools: ToolRegistry::new(),
        config: RunnerConfig {
            model: "parent-model".into(),
            ..RunnerConfig::default()
        },
        subagent_type: "explore".to_string(),
        prompt: "find the auth module".to_string(),
        prompt_summary: "find the auth module".to_string(),
        trigger: TriggerSource::ModelTool {
            tool_call_id: "test-call".to_string(),
        },
        depth,
        isolation: IsolationMode::None,
        worktree_controller: None,
        run_mode: ChildRunMode::Foreground,
        child_cancel: CancellationToken::new(),
        bus: None,
        descriptor_store: None,
        resume: None,
        timeout_secs: None,
        skill_registry: None,
        conditional: None,
        hooks: None,
        inherited: RunnerInheritance::default(),
        partial_transcript: None,
    }
}

/// spawn_child creates a child session id distinct from the parent,
/// records a SubagentSpawn durable boundary in the parent log, and
/// returns a ChildHandle carrying the child session id + cancel token.
#[tokio::test]
async fn test_spawn_creates_boundary() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let req = req_at_depth(parent_sid, store.clone(), provider, 0);
    let handle = spawn_child(req).await.expect("spawn should succeed");
    assert_ne!(handle.session, parent_sid);

    // The child runner carries depth + 1 and the resolved type, so a
    // nested agent call reports its level to the recursion guard.
    let identity = handle.runner.agent_identity();
    assert_eq!(identity.depth, 1, "child depth must be parent + 1");
    assert_eq!(
        identity.subagent_type.as_deref(),
        Some("explore"),
        "child identity must carry the resolved type",
    );
    assert_eq!(
        identity.parent_session_id.as_deref(),
        Some(parent_sid.to_string().as_str()),
    );

    // The parent log must carry a SubagentSpawn boundary whose
    // child_session_id matches the handle's session id. Resume and orphan
    // reconciliation pair spawn with return on this id; a mismatched or
    // missing id breaks the durable chain.
    let parent_events = store.trajectory_tail(parent_sid);
    let spawn = parent_events
        .iter()
        .find(|e| matches!(e.event, SessionEvent::SubagentSpawn { .. }))
        .expect("parent log must carry the spawn boundary");
    let (recorded_child, recorded_trigger) = match &spawn.event {
        SessionEvent::SubagentSpawn {
            child_session_id,
            trigger_source,
            ..
        } => (child_session_id.clone(), trigger_source.clone()),
        _ => unreachable!("matched above"),
    };
    assert_eq!(
        recorded_child,
        handle.session.to_string(),
        "spawn boundary must record the child session id"
    );
    assert_eq!(
        recorded_trigger, "model:test-call",
        "spawn boundary must record the trigger source (a model delegation)"
    );
}

/// The child's own log opens with the delegation it came from: the parent the
/// boundary names. Written at the boundary rather than after the run, so a
/// child whose descriptor never landed still says whose it is, and a reader can
/// name its parent without the descriptor.
#[tokio::test]
async fn test_delegation_opens_child_log() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let req = req_at_depth(parent_sid, store.clone(), provider, 0);
    let handle = spawn_child(req).await.expect("spawn should succeed");
    let events = store.trajectory_tail(handle.session);
    match &events.first().expect("child log is not empty").event {
        SessionEvent::ChildDelegated {
            parent_session_id,
            subagent_type,
        } => {
            assert_eq!(parent_session_id, &parent_sid.to_string());
            assert_eq!(subagent_type, "explore");
        }
        other => panic!("the child log must open with its delegation, got {other:?}"),
    }
}

/// The child runner records its own prompt. The child's first durable append
/// materializes a descriptor through the shared store, but only the child
/// runner observes the prompt it runs on; without the parent's descriptor
/// store the child's descriptor never learns what the child was asked.
#[tokio::test]
async fn test_child_records_prompt() {
    let descriptors = Arc::new(InMemoryDescriptorStore::new());
    let materialized = Arc::clone(&descriptors);
    let store = Arc::new(
        SessionStore::new(Box::new(InMemoryBackend::new())).with_first_durable(Arc::new(
            move |sid| {
                let descriptor = SessionDescriptor::delegated_child(
                    SessionProvenance::Fresh,
                    0,
                    PromptCoverage::Complete,
                );
                materialized
                    .write_descriptor(sid, &descriptor)
                    .expect("materialize");
            },
        )),
    );
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("done"));
    let mut req = req_at_depth(parent_sid, store, provider, 0);
    let descriptor_store: Arc<dyn SessionDescriptorStore> = descriptors.clone();
    req.descriptor_store = Some(descriptor_store);
    let handle = spawn_child(req).await.expect("spawn");
    handle
        .runner
        .run(handle.session, "find the auth module".to_string())
        .await
        .expect("the child turn runs");
    handle.runner.settle_prompt_writes().await;
    let descriptor = descriptors
        .read_descriptor(handle.session)
        .expect("the child descriptor exists");
    assert_eq!(
        descriptor
            .first_prompt
            .expect("the child's first prompt is recorded")
            .text,
        "find the auth module"
    );
    assert_eq!(
        descriptor
            .last_prompt
            .expect("the child's last prompt follows")
            .text,
        "find the auth module"
    );
}

/// A system-triggered spawn records its origin on the durable boundary.
#[tokio::test]
async fn test_spawn_records_system_trigger() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let mut req = req_at_depth(parent_sid, store.clone(), provider, 0);
    req.trigger = TriggerSource::System {
        hook: "review_gate".into(),
    };
    let handle = spawn_child(req).await.expect("spawn should succeed");
    let events = store.trajectory_tail(parent_sid);
    let spawn = events
        .iter()
        .find(|e| matches!(e.event, SessionEvent::SubagentSpawn { .. }))
        .expect("parent log must carry the spawn boundary");
    let recorded_trigger = match &spawn.event {
        SessionEvent::SubagentSpawn { trigger_source, .. } => trigger_source.clone(),
        _ => unreachable!("matched above"),
    };
    assert_eq!(
        recorded_trigger, "system:review_gate",
        "system-triggered spawn records its hook origin, not a model delegation"
    );
    // The child is spawned regardless of the trigger origin; the capability
    // baseline is the parent's, so a system trigger does not bypass the
    // capability intersection rule. The child identity still carries depth+1.
    assert_eq!(
        handle.runner.agent_identity().depth,
        1,
        "system spawn still produces a depth-tracked child"
    );
}

/// The recursion guard rejects a spawn at the depth cap before any side
/// effect: no child session is created, no boundary is written to the
/// parent log. A reject that left a dangling boundary would break resume
/// (an unpaired spawn the parent never ran).
#[tokio::test]
async fn test_spawn_rejects_depth_cap() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let req = req_at_depth(parent_sid, store.clone(), provider, 4);
    match spawn_child(req).await {
        Ok(_) => panic!("spawn at depth cap must reject, not succeed"),
        Err(e) => assert_eq!(e, SpawnError::SpawnRecursive),
    }
    let parent_events = store.trajectory_tail(parent_sid);
    assert!(
        parent_events.is_empty(),
        "a rejected spawn must write no boundary: {:?}",
        parent_events
    );
}

/// The topology is exactly two levels: a top-level agent may spawn, and its
/// child may not spawn again.
#[tokio::test]
async fn test_spawn_depth_two_levels() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let req = req_at_depth(parent_sid, store.clone(), provider, 0);
    let handle = spawn_child(req).await.expect("a top-level agent may spawn");
    assert_eq!(
        handle.runner.agent_identity().depth,
        1,
        "the child is the last level"
    );

    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let nested = req_at_depth(SessionId::new(), store, provider, 1);
    assert!(
        matches!(spawn_child(nested).await, Err(SpawnError::SpawnRecursive)),
        "a child may not spawn again"
    );
}

/// spawn_child stores the caller-supplied cancel token on the handle
/// verbatim, so firing the supplied token cancels the handle. Linkage
/// (a foreground child sharing the parent token vs a background child getting
/// a fresh one) is the service layer's policy, chosen before spawn, not the
/// core's.
#[tokio::test]
async fn test_spawn_uses_passed_cancel() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let token = CancellationToken::new();
    let mut req = req_at_depth(parent_sid, store, provider, 0);
    req.child_cancel = token.clone();
    let handle = spawn_child(req).await.expect("spawn");
    token.cancel();
    assert!(
        handle.cancel.is_cancelled(),
        "the child's cancel is the caller-supplied token verbatim"
    );
}

/// A spawned child's requests carry the lowest effort tier, so a fan-out of
/// sub-agents does not multiply reasoning-token spend.
#[tokio::test]
async fn test_spawn_child_effort_gate() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let req = req_at_depth(parent_sid, store, provider, 0);
    let handle = spawn_child(req).await.expect("spawn");
    assert_eq!(
        handle.runner.active_effort(),
        Some(EffortLevel::Low),
        "child uses the lowest effort tier"
    );
}

/// A child publishes one progress snapshot after each non-terminal turn.
#[tokio::test]
async fn test_spawn_publishes_progress() {
    let bus = Arc::new(AgentBus::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    // Turn 1: a tool call → resolve_turn returns RunAgain → progress emitted.
    // Turn 2: final text → FinalOutput → terminal, no progress.
    let resp1 = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "grep".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Usage::default()
        },
        model: "test".into(),
    };
    let resp2 = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
            ..Usage::default()
        },
        model: "test".into(),
    };
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![resp1, resp2]));
    let mut req = req_at_depth(parent_sid, store, provider, 0);
    req.bus = Some(bus.clone());
    let handle = spawn_child(req).await.expect("spawn");

    // Subscribe before the run: broadcast drops messages a late subscriber
    // never saw. The parent subscribes at spawn time (before the run starts).
    let child_id = handle.session.to_string();
    let mut rx = bus.subscribe(&progress_topic(&child_id));

    // Drive the child to terminal. The publish fires mid-run, but the
    // broadcast buffer holds it for the receiver to drain after.
    let _result = handle
        .runner
        .run(handle.session, "do the task".to_string())
        .await;

    // The turn-1 boundary published one Progress the parent received.
    match rx.try_recv().expect("parent received progress") {
        BusMessage::Progress {
            child,
            turn,
            tokens,
            tool_uses,
            last_activity,
        } => {
            assert_eq!(child.agent_id, child_id);
            assert_eq!(turn, 1);
            assert_eq!(tokens, 150); // 100 input + 50 output, cumulative after turn 1
            assert_eq!(tool_uses, 1);
            assert_eq!(last_activity.as_deref(), Some("grep"));
        }
        other => panic!("expected Progress, got {other:?}"),
    }
    // Only one turn boundary was crossed; no second Progress.
    assert!(
        rx.try_recv().is_err(),
        "no second progress for a 2-turn run"
    );
}

/// Adversarial: a child that completes in a single turn (text only, no
/// tool calls) crosses no turn boundary, so it publishes no Progress — a
/// parent that saw a message here would mean the guard fires on terminal turns
/// too, flooding the bus with useless completion-coupled progress.
#[tokio::test]
async fn test_spawn_terminal_skips_progress() {
    let bus = Arc::new(AgentBus::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("immediate answer"));
    let mut req = req_at_depth(parent_sid, store, provider, 0);
    req.bus = Some(bus.clone());
    let handle = spawn_child(req).await.expect("spawn");
    let child_id = handle.session.to_string();
    let mut rx = bus.subscribe(&progress_topic(&child_id));
    let _result = handle
        .runner
        .run(handle.session, "do the task".to_string())
        .await;
    // No turn boundary crossed (single turn → FinalOutput), so no Progress.
    assert!(
        rx.try_recv().is_err(),
        "single-turn child must not publish progress"
    );
}

/// A child that completes publishes a Completed message on its completed
/// topic, so a parent subscribed before the run learns the child is done.
#[tokio::test]
async fn test_spawn_publishes_completion() {
    let bus = Arc::new(AgentBus::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("found auth module"));
    let mut req = req_at_depth(parent_sid, store, provider, 0);
    req.bus = Some(bus.clone());
    let handle = spawn_child(req).await.expect("spawn");
    let child_id = handle.session.to_string();
    let mut rx = bus.subscribe(&completed_topic(&child_id));
    let _result = handle
        .runner
        .run(handle.session, "do the task".to_string())
        .await;
    match rx.try_recv().expect("parent received completion") {
        BusMessage::Completed {
            child,
            status,
            summary,
        } => {
            assert_eq!(child.agent_id, child_id);
            assert_eq!(status, CompletionStatus::Completed);
            assert_eq!(summary, "found auth module");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// A child drains its bus inbox at each turn boundary: a text the parent
/// sent to the child's inbox lands as a MidTurnInput event before the next
/// model call. The mpsc inbox is point-to-point, so the message queued
/// after spawn + before run is drained at turn 2 — no race.
#[tokio::test]
async fn test_inbox_drained_at_boundary() {
    let bus = Arc::new(AgentBus::new());
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let turn1 = CompletionResponse {
        output: vec![OutputItem::ToolCall {
            id: "c1".into(),
            name: "grep".into(),
            input: serde_json::json!({}),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let turn2 = CompletionResponse {
        output: vec![OutputItem::Text {
            text: "done".into(),
        }],
        usage: Usage::default(),
        model: "test".into(),
    };
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(vec![turn1, turn2]));
    let mut req = req_at_depth(parent_sid, store.clone(), provider, 0);
    req.bus = Some(bus.clone());
    let handle = spawn_child(req).await.expect("spawn");
    let child_id = handle.session.to_string();
    // Parent steers the child before its second turn; the inbox is
    // registered at spawn so this succeeds, and the drive loop drains it
    // at the turn-2 boundary.
    bus.send_inbox(
        &child_id,
        BusMessage::Inbox {
            text: "steer here".into(),
        },
    )
    .expect("inbox registered at spawn");
    let _result = handle
        .runner
        .run(handle.session, "do the task".to_string())
        .await;
    let events = store.trajectory_tail(handle.session);
    assert!(
        events.iter().any(|e| matches!(&e.event,
                SessionEvent::MidTurnInput { text, .. } if text == "steer here")),
        "inbox text drained at the turn boundary as a MidTurnInput event"
    );
}

/// A tiny registry holding one skill, so a test can tell "the child has the
/// parent's skill set" from "the child has no skill set at all".
struct OneSkill;

impl SkillRegistry for OneSkill {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        vec![SkillDescriptor {
            name: "deep-review".into(),
            description: "review a change".into(),
            when_to_use: None,
            argument_hint: None,
            disable_model_invocation: false,
            user_invocable: true,
            body_token_estimate: 0,
            allowed_tools: Vec::new(),
            allowed_mach_services: Vec::new(),
            allow_app_launch: false,
        }]
    }

    fn find(&self, _name: &str) -> Option<SkillDescriptor> {
        None
    }

    fn prepare_body(
        &self,
        name: &str,
        _args: Option<&str>,
        _session_id: Option<&str>,
    ) -> Result<String, SkillError> {
        Err(SkillError::NotFound(name.to_string()))
    }
}

/// The child runner carries the parent's skill registry. The child holds the
/// Skill tool, so a runner without the registry has a tool with no listing
/// and no resolver behind it: it cannot see which skills exist, and the model
/// falls back to reading a SKILL.md by absolute path.
#[tokio::test]
async fn test_child_inherits_skill_registry() {
    let registry: Arc<dyn SkillRegistry> = Arc::new(OneSkill);
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let mut req = req_at_depth(parent_sid, store.clone(), provider, 0);
    req.skill_registry = Some(Arc::clone(&registry));

    let handle = spawn_child(req).await.expect("spawn");

    let names: Vec<String> = handle
        .runner
        .skill_registry()
        .expect("the child carries the parent's registry")
        .list_model_invocable()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(names, vec!["deep-review".to_string()]);
}

/// A project rule that refuses every call about to run. The child inherits
/// the registry holding it, so its own tool calls meet the same gate.
struct RefuseAll;

impl Hook for RefuseAll {
    fn name(&self) -> &str {
        "refuse-all"
    }
    fn events(&self) -> &[HookEvent] {
        &[HookEvent::PreToolUse]
    }
    fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
        Ok(HookVerdict::Deny("project rule".into()))
    }
    fn source(&self) -> HookSource {
        HookSource::Project
    }
}

/// A child inherits the parent's hook registry, so its tool calls answer to
/// the same write-time gates. A child without it is a path around every
/// project rule: the parent's gates apply where the parent runs tools, and
/// the child runs its own.
#[tokio::test]
async fn test_child_inherits_hooks() {
    let hooks = Arc::new(HookRegistry::new());
    hooks.register(Arc::new(RefuseAll));
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let parent_sid = SessionId::new();
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let mut req = req_at_depth(parent_sid, store.clone(), provider, 0);
    req.hooks = Some(Arc::clone(&hooks));

    let handle = spawn_child(req).await.expect("spawn");

    let names: Vec<String> = handle
        .runner
        .hooks_list()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    assert_eq!(
        names,
        vec!["refuse-all".to_string()],
        "the child answers to the parent's gates"
    );
}

/// A resume continues the named child session instead of minting a new one.
/// The child's earlier turns are durable in that session's log, so this is
/// what makes an interrupted delegation recoverable rather than a full rerun.
#[tokio::test]
async fn test_resume_keeps_child_session() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let parent = SessionId::new();
    let first = spawn_child(req_at_depth(
        parent,
        store.clone(),
        Arc::clone(&provider),
        0,
    ))
    .await
    .expect("spawn");
    let existing = first.session;
    drop(first);
    let mut req = req_at_depth(parent, store.clone(), provider, 0);
    req.resume = Some(existing);

    let handle = spawn_child(req).await.expect("spawn");

    assert_eq!(
        handle.session, existing,
        "the resumed child keeps the session it was asked to continue"
    );
    let delegations = store
        .trajectory_tail(existing)
        .iter()
        .filter(|entry| matches!(entry.event, SessionEvent::ChildDelegated { .. }))
        .count();
    assert_eq!(
        delegations, 1,
        "a resumed child keeps the delegation record its log already holds"
    );
}

/// A resume target that does not exist, or that exists but was started by a
/// different parent, is refused instead of silently becoming a fresh child:
/// continuing the parent's own session or another conversation would write a
/// delegated turn into a log nobody delegated.
#[tokio::test]
async fn test_resume_rejects_invalid_target() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let parent = SessionId::new();
    let mut missing = req_at_depth(parent, store.clone(), Arc::clone(&provider), 0);
    missing.resume = Some(SessionId::new());
    let error = spawn_child(missing)
        .await
        .err()
        .expect("an unknown id is refused");
    assert!(
        matches!(error, SpawnError::InvalidResumeTarget { .. }),
        "the refusal names the resume target: {error:?}"
    );

    let other = spawn_child(req_at_depth(
        SessionId::new(),
        store.clone(),
        Arc::clone(&provider),
        0,
    ))
    .await
    .expect("another parent's child spawns");
    let other_session = other.session;
    drop(other);
    let mut foreign = req_at_depth(parent, store.clone(), provider, 0);
    foreign.resume = Some(other_session);
    let error = spawn_child(foreign)
        .await
        .err()
        .expect("a foreign child is refused");
    assert!(
        matches!(error, SpawnError::InvalidResumeTarget { .. }),
        "the refusal names the resume target: {error:?}"
    );
}

/// A resume target that is a real conversation but not a delegation of this
/// parent is refused: the parent's own session and another top-level session
/// both exist and have turns, but neither was delegated by this parent, so
/// continuing either would write a delegated turn into a conversation nobody
/// delegated. No spawn boundary is written into the target's own log.
#[tokio::test]
async fn test_resume_rejects_non_delegation() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let parent = SessionId::new();
    seed_turns(store.as_ref(), parent).await;
    let mut own = req_at_depth(parent, store.clone(), Arc::clone(&provider), 0);
    own.resume = Some(parent);
    let error = spawn_child(own)
        .await
        .err()
        .expect("the parent's own session is not a delegation");
    assert!(
        matches!(error, SpawnError::InvalidResumeTarget { .. }),
        "the refusal names the resume target: {error:?}"
    );
    assert!(
        store
            .trajectory_tail(parent)
            .iter()
            .all(|e| !matches!(e.event, SessionEvent::SubagentSpawn { .. })),
        "no spawn boundary is written into the parent's own session"
    );

    let top = SessionId::new();
    seed_turns(store.as_ref(), top).await;
    let mut foreign = req_at_depth(parent, store.clone(), provider, 0);
    foreign.resume = Some(top);
    let error = spawn_child(foreign)
        .await
        .err()
        .expect("a top-level session is not a delegation");
    assert!(
        matches!(error, SpawnError::InvalidResumeTarget { .. }),
        "the refusal names the resume target: {error:?}"
    );
}

/// A spawn with no resume creates a child session that is not the one the
/// parent runs in and not a session the caller named, so the field is opt-in
/// and a plain delegation starts clean.
#[tokio::test]
async fn test_plain_spawn_creates_session() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let parent = SessionId::new();
    let req = req_at_depth(parent, store.clone(), provider, 0);

    let handle = spawn_child(req).await.expect("spawn");

    assert_ne!(handle.session, parent, "the child is its own session");
}

/// A summarizer that answers with one fixed text, standing in for the
/// provider-backed one the composition root installs.
struct FixedText(&'static str);

impl Summarizer for FixedText {
    fn summarize<'a>(
        &'a self,
        _events: &'a [SessionLogEntry],
        _custom_instructions: Option<&'a str>,
    ) -> PFut<'a, Result<String, SummarizeError>> {
        let text = self.0.to_string();
        Box::pin(async move { Ok(text) })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Append a few turns to a session so its log has something to fold.
async fn seed_turns(store: &SessionStore, session: SessionId) {
    for i in 0..6 {
        store
            .append(SessionLogEntry {
                id: EventId::new(),
                session,
                ts: i,
                prev_hash: None,
                event: if i == 0 {
                    SessionEvent::UserInput {
                        text: "do the work".into(),
                    }
                } else {
                    SessionEvent::AssistantMessage {
                        text: format!("step {i}"),
                        thinking: None,
                    }
                },
            })
            .await
            .expect("seed turn");
    }
}

/// A child compresses its own log with the summarizer its parent was built
/// with, not the engine's heuristic default: a delegation that outgrows its
/// window keeps the quality of summary a top-level turn would get.
#[tokio::test]
async fn test_child_compacts_with_llm() {
    let store = Arc::new(SessionStore::new(Box::new(InMemoryBackend::new())));
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::text("ok"));
    let mut req = req_at_depth(SessionId::new(), store.clone(), provider, 0);
    req.inherited = RunnerInheritance {
        summarizer: Some(Arc::new(FixedText("the child's own summary"))),
        ..RunnerInheritance::default()
    };

    let handle = spawn_child(req).await.expect("spawn");
    seed_turns(store.as_ref(), handle.session).await;

    let outcome = handle
        .runner
        .compact(handle.session)
        .await
        .expect("the child compacts its own log");
    let manifest = store
        .read_checkpoint(outcome.manifest_id)
        .await
        .expect("checkpoint written");
    let summary = manifest.summary.expect("summary present");
    assert!(
        summary.contains("the child's own summary"),
        "the child folded its log with the inherited summarizer: {summary}"
    );
}
