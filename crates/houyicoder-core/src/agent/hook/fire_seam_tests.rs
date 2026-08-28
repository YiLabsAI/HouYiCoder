//! HookFire seam tests: the HookDispatcher fires the four service-fired
//! reserved events (SubagentStart, SubagentStop, WorktreeCreate,
//! WorktreeRemove), dispatching configured hooks and recording one HookSignal
//! per event in the parent session log. build_hook_fire returns None when no
//! registry is wired. Non-service events are a no-op (no panic).

use std::sync::{Arc, Mutex};

use houyicoder_context::{HookEventKind, HookFirePayload, SessionId, TurnEvent, TurnEventKind};
use houyicoder_protocol::llm::{CompletionResponse, OutputItem, Usage};

use crate::agent::ToolRegistry;
use crate::agent::hook::HookRegistry;
use crate::agent::hook::{Hook, HookContext, HookError, HookEvent, HookSource, HookVerdict};
use crate::agent::tests::runner_with;
use crate::agent::{Runner, build_hook_fire};
use crate::provider::test_support::FakeProvider;

/// A hook subscribed to the four reserved events, returning Observe so each
/// dispatch lands a durable HookSignal (Allow is skipped). Records the core
/// HookEvent it saw so the test asserts dispatch per event.
struct ReservedRecorder {
    seen: Arc<Mutex<Vec<HookEvent>>>,
}
impl Hook for ReservedRecorder {
    fn name(&self) -> &str {
        "reserved-recorder"
    }
    fn events(&self) -> &[HookEvent] {
        &[
            HookEvent::SubagentStart,
            HookEvent::SubagentStop,
            HookEvent::WorktreeCreate,
            HookEvent::WorktreeRemove,
        ]
    }
    fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError> {
        self.seen.lock().expect("recorder lock").push(ctx.event);
        Ok(HookVerdict::Observe("reserved".into()))
    }
    fn source(&self) -> HookSource {
        HookSource::Project
    }
}

fn runner_with_reserved_hook() -> (Runner, Arc<Mutex<Vec<HookEvent>>>) {
    let seen = Arc::new(Mutex::new(Vec::<HookEvent>::new()));
    let registry = HookRegistry::new();
    registry.register(Arc::new(ReservedRecorder { seen: seen.clone() }));
    let runner = runner_with(
        Arc::new(FakeProvider::new(vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }])),
        ToolRegistry::new(),
    )
    .with_hooks(Arc::new(registry));
    (runner, seen)
}

fn runner_no_hooks() -> Runner {
    runner_with(
        Arc::new(FakeProvider::new(vec![CompletionResponse {
            output: vec![OutputItem::Text {
                text: "done".into(),
            }],
            usage: Usage::default(),
            model: "test".into(),
        }])),
        ToolRegistry::new(),
    )
}

/// Fire each of the four service-fired events; the dispatcher maps the leaf
/// kind to the typed core HookContext, dispatches the configured hook, and
/// records one HookSignal per event in the parent session log with the
/// matching wire event kind.
#[tokio::test]
async fn test_fire_dispatches_reserved_events() {
    let (runner, seen) = runner_with_reserved_hook();
    let Some(hook_fire) = build_hook_fire(&runner) else {
        panic!("build_hook_fire must return Some when hooks are wired");
    };
    let session = SessionId::new();
    hook_fire
        .fire(
            HookEventKind::SubagentStart,
            HookFirePayload::subagent_start(session, "child-1".into(), "explore".into()),
        )
        .await;
    hook_fire
        .fire(
            HookEventKind::SubagentStop,
            HookFirePayload::subagent_stop(
                session,
                "child-1".into(),
                "explore".into(),
                "completed".into(),
                Some("answer".into()),
            ),
        )
        .await;
    hook_fire
        .fire(
            HookEventKind::WorktreeCreate,
            HookFirePayload::worktree(session, "/tmp/wt-a".into()),
        )
        .await;
    hook_fire
        .fire(
            HookEventKind::WorktreeRemove,
            HookFirePayload::worktree(session, "/tmp/wt-a".into()),
        )
        .await;

    let recorded = seen.lock().expect("seen lock").clone();
    assert_eq!(recorded.len(), 4, "all four reserved events dispatched");
    assert!(recorded.contains(&HookEvent::SubagentStart));
    assert!(recorded.contains(&HookEvent::SubagentStop));
    assert!(recorded.contains(&HookEvent::WorktreeCreate));
    assert!(recorded.contains(&HookEvent::WorktreeRemove));

    let snap = runner.store().trajectory_snapshot(session);
    let signals: Vec<&TurnEvent> = snap
        .iter()
        .filter(|ev| matches!(ev.kind, TurnEventKind::HookSignal { .. }))
        .collect();
    assert_eq!(signals.len(), 4, "one HookSignal per fired event");
    let wire_kinds: Vec<HookEventKind> = signals
        .iter()
        .filter_map(|ev| match &ev.kind {
            TurnEventKind::HookSignal { event, .. } => Some(*event),
            _ => None,
        })
        .collect();
    assert!(wire_kinds.contains(&HookEventKind::SubagentStart));
    assert!(wire_kinds.contains(&HookEventKind::SubagentStop));
    assert!(wire_kinds.contains(&HookEventKind::WorktreeCreate));
    assert!(wire_kinds.contains(&HookEventKind::WorktreeRemove));
}

/// build_hook_fire returns None when the runner has no hook registry wired,
/// so a no-hook dispatch treats fire as a no-op rather than constructing a
/// dispatcher against an empty registry.
#[tokio::test]
async fn test_hookfire_none_without_hooks() {
    let runner = runner_no_hooks();
    assert!(build_hook_fire(&runner).is_none(), "no hooks wired -> None");
}

/// The seam ignores events it does not fire (the 24 non-service events): a
/// caller passing SessionStart gets a no-op, no panic, no signal.
#[tokio::test]
async fn test_hookfire_ignores_nonservice() {
    let (runner, _seen) = runner_with_reserved_hook();
    let hook_fire = build_hook_fire(&runner).expect("wired");
    let session = SessionId::new();
    hook_fire
        .fire(
            HookEventKind::SessionStart,
            HookFirePayload {
                session,
                agent_id: None,
                agent_type: None,
                status: None,
                last_text: None,
                path: None,
            },
        )
        .await;
    let snap = runner.store().trajectory_snapshot(session);
    let signals = snap
        .iter()
        .filter(|ev| matches!(ev.kind, TurnEventKind::HookSignal { .. }))
        .count();
    assert_eq!(signals, 0, "non-service event is a no-op");
}
