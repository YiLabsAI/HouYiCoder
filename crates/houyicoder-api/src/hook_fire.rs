//! The hook-fire seam: a leaf-side trait a service-layer boundary calls to
//! fire a reserved lifecycle hook. One method takes the existing
//! HookEventKind mirror (context) plus a flat HookFirePayload, so adding a
//! service-fired event is a new payload field, not a new trait method. The
//! core HookDispatcher implements this; it dispatches the configured hooks
//! and records a durable HookSignal in the parent session log. The trait
//! lives in the api leaf (which cannot depend on the richer core HookEvent),
//! so it references only the leaf mirror plus the payload. The exhaustive
//! compile-time guarantee lives in the wire direction (core HookEvent to
//! leaf HookEventKind); the reverse mapping in the dispatcher (leaf to core)
//! is partial — only the events this seam fires map, the rest are a no-op,
//! so adding a service-fired event also needs a dispatcher arm (not caught by
//! the mirror check).

use houyicoder_async::PFut;
use houyicoder_context::{HookEventKind, HookFirePayload};

/// Fire a reserved lifecycle hook from a service-layer boundary (a subagent
/// spawn or return in run_sync_spawn, a worktree enter or exit in the
/// worktree controller). The implementor dispatches the configured hooks for
/// the event and appends a per-hook HookSignal to the session log so the fire
/// is replayable. The default impl is a no-op so a stub dispatch or a test
/// with no registry wired pays nothing.
pub trait HookFire: Send + Sync {
    fn fire(&self, event: HookEventKind, payload: HookFirePayload) -> PFut<'_, ()> {
        drop((event, payload));
        Box::pin(async {})
    }
}
