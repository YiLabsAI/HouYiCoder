//! The HookFire implementor a service-layer fire point calls. Holds Arc
//! clones of the parent runner's hook deps (registry, store, observability,
//! live sink), not a reference back to the runner, so firing from a
//! service-layer boundary (run_sync_spawn, the worktree controller) records
//! with the same shape the in-loop hook recorder does, with no
//! self-referential Arc cycle. build_hook_fire reads the runner's private
//! fields (visible to this descendant module) and returns None when no
//! registry is wired, so a no-hook dispatch is a no-op rather than a panic.

use std::sync::Arc;

use houyicoder_api::hook_fire::HookFire;
use houyicoder_api::live::LiveSink;
use houyicoder_api::session::SessionLog;
use houyicoder_async::PFut;
use houyicoder_context::{HookEventKind, HookFirePayload};

use super::{HookContext, HookEvent, HookPayload, HookRegistry};
use crate::agent::Runner;
use crate::agent::append::{emit_live_line, record_hook_signals};
use crate::agent::obs_wire::SharedObservability;
use houyicoder_context::AgentId;

/// The HookFire implementor: a bundle of the parent runner's hook deps as
/// Arc clones. Built once at the composition root after the runner and its
/// hook registry exist, then threaded to the service-layer fire points. fire
/// maps the leaf event kind plus flat payload to the typed core HookContext,
/// dispatches the configured hooks, and records one HookSignal per outcome to
/// the parent session log through the same record_hook_signals the Runner's
/// in-loop fire path uses, so the two paths can never diverge.
pub(crate) struct HookDispatcher {
    hooks: Arc<HookRegistry>,
    store: Arc<dyn SessionLog>,
    obs: SharedObservability,
    live: Option<LiveSink>,
}

impl HookDispatcher {
    pub(crate) fn new(
        hooks: Arc<HookRegistry>,
        store: Arc<dyn SessionLog>,
        obs: SharedObservability,
        live: Option<LiveSink>,
    ) -> Self {
        Self {
            hooks,
            store,
            obs,
            live,
        }
    }
}

impl HookFire for HookDispatcher {
    fn fire(&self, event: HookEventKind, payload: HookFirePayload) -> PFut<'_, ()> {
        Box::pin(async move {
            let Some(ctx) = build_context(event, &payload) else {
                return;
            };
            let outcomes = self.hooks.dispatch(&ctx);
            if let Some(skipped) = self.hooks.take_skipped_untrusted() {
                emit_live_line(
                    self.live.as_ref(),
                    format!("untrusted project hooks skipped: {}", skipped.join(", ")),
                );
            }
            record_hook_signals(
                self.store.as_ref(),
                &self.obs,
                self.live.as_ref(),
                payload.session,
                ctx.event,
                None,
                &outcomes,
            )
            .await;
        })
    }
}

/// Build the typed HookContext the registry dispatches against, from the
/// leaf event kind plus flat payload. Only the four service-fired events
/// map; any other kind is a caller bug (the seam does not fire them) and
/// returns None so the fire is a no-op rather than a panic.
fn build_context(event: HookEventKind, payload: &HookFirePayload) -> Option<HookContext> {
    let session = payload.session;
    let (event, hp) = match event {
        HookEventKind::SubagentStart => {
            let agent_id = AgentId(payload.agent_id.clone().unwrap_or_default());
            let agent_type = payload.agent_type.clone().unwrap_or_default();
            (
                HookEvent::SubagentStart,
                HookPayload::SubagentStart {
                    agent_id,
                    agent_type,
                },
            )
        }
        HookEventKind::SubagentStop => {
            let agent_id = AgentId(payload.agent_id.clone().unwrap_or_default());
            let agent_type = payload.agent_type.clone().unwrap_or_default();
            let status = payload.status.clone().unwrap_or_default();
            let last_text = payload.last_text.clone();
            (
                HookEvent::SubagentStop,
                HookPayload::SubagentStop {
                    agent_id,
                    agent_type,
                    status,
                    last_text,
                },
            )
        }
        HookEventKind::WorktreeCreate => {
            let path = payload.path.clone().unwrap_or_default();
            (
                HookEvent::WorktreeCreate,
                HookPayload::WorktreeCreate { path },
            )
        }
        HookEventKind::WorktreeRemove => {
            let path = payload.path.clone().unwrap_or_default();
            (
                HookEvent::WorktreeRemove,
                HookPayload::WorktreeRemove { path },
            )
        }
        _ => return None,
    };
    Some(HookContext {
        event,
        payload: hp,
        session,
    })
}

/// Build the HookFire a service-layer fire point calls, from the parent
/// runner's hook deps. Returns None when the runner has no hook registry
/// wired (a test, a stub run, or the depth-1-only v0 child that carries no
/// registry); the caller treats fire as a no-op then. This descendant module
/// reads the runner's private fields directly, so no leaky getters cross the
/// crate boundary.
pub fn build_hook_fire(runner: &Runner) -> Option<Arc<dyn HookFire>> {
    let hooks = runner.hooks.as_ref()?;
    Some(Arc::new(HookDispatcher::new(
        Arc::clone(hooks),
        runner.store.clone(),
        runner.observability.clone(),
        runner.live.clone(),
    )))
}
