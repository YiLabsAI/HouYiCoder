//! HookRegistry: registration, dispatch, and policy filtering. Split
//! from hook.rs so that file stays under the file-size gate; the
//! registry + list visibility method live here.

use super::*;
use houyicoder_api::trust::TrustState;
use std::sync::RwLock;

/// A stable handle for a registered hook. Stable across removals (unlike a
/// Vec index), so a caller can hold the id and unregister later — the
/// once:true skill-hook pattern drops itself after its first success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HookId(u64);

/// Mutable inner state. The hook map is keyed by HookId so removal is O(1)
/// and a stale id never dereferences a moved-aside slot; order preserves
/// registration order for dispatch + list; by_event indexes events to
/// HookIds (not indices) so a removal just filters the lists. All three
/// are mutated together on register/unregister under one lock.
struct HookInner {
    hooks: HashMap<HookId, Arc<dyn Hook>>,
    order: Vec<HookId>,
    by_event: HashMap<HookEvent, Vec<HookId>>,
    next_id: u64,
}

impl HookInner {
    fn new() -> Self {
        Self {
            hooks: HashMap::new(),
            order: Vec::new(),
            by_event: HashMap::new(),
            next_id: 0,
        }
    }
}

pub struct HookRegistry {
    inner: RwLock<HookInner>,
    policy: HookPolicy,
    trust: TrustState,
    timeout_ms: u64,
    pending_skip_notice: Mutex<Option<Vec<String>>>,
}

impl HookRegistry {
    /// Create an empty registry with the default (all-enabled) policy and
    /// trusted state.
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HookInner::new()),
            policy: HookPolicy::default(),
            trust: TrustState::Trusted,
            timeout_ms: 5000,
            pending_skip_notice: Mutex::new(None),
        }
    }

    pub fn with_policy(policy: HookPolicy) -> Self {
        Self {
            inner: RwLock::new(HookInner::new()),
            policy,
            trust: TrustState::Trusted,
            timeout_ms: 5000,
            pending_skip_notice: Mutex::new(None),
        }
    }

    pub fn with_policy_and_trust(policy: HookPolicy, trust: TrustState) -> Self {
        Self {
            inner: RwLock::new(HookInner::new()),
            policy,
            trust,
            timeout_ms: 5000,
            pending_skip_notice: Mutex::new(None),
        }
    }

    /// Set the registry-wide evaluate timeout (applies to all hooks at
    /// dispatch; per-hook timeout is not supported yet). 0 disables
    /// timeout (sequential dispatch). Returns self for chaining.
    pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Take the pending untrusted-skip notice, if any. Once drained the
    /// registry stays silent: trust is fixed at construction, so the skip
    /// set cannot change over this registry's lifetime.
    pub fn take_skipped_untrusted(&self) -> Option<Vec<String>> {
        self.pending_skip_notice
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Register a hook. Takes &self (interior mutability via RwLock)
    /// so hooks can be registered on a shared Arc<HookRegistry>
    /// after the runner is constructed. This enables future
    /// skill-hook registration at invocation time. Returns a stable
    /// HookId the caller can hold to unregister later.
    pub fn register(&self, hook: Arc<dyn Hook>) -> HookId {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let id = HookId(inner.next_id);
        inner.next_id += 1;
        for ev in hook.events() {
            inner.by_event.entry(*ev).or_default().push(id);
        }
        inner.order.push(id);
        inner.hooks.insert(id, hook);
        id
    }

    /// Remove a hook by its stable handle. Returns false if the id is
    /// not registered (already removed, or never minted). Filters the
    /// id out of the order list and every event index so dispatch and
    /// list never see a stale id. The once:true skill-hook pattern
    /// unregisters itself after its first successful fire.
    pub fn unregister(&self, id: HookId) -> bool {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        if inner.hooks.remove(&id).is_none() {
            return false;
        }
        inner.order.retain(|x| *x != id);
        for ids in inner.by_event.values_mut() {
            ids.retain(|x| *x != id);
        }
        true
    }

    /// Dispatch an event to all subscribed hooks, collecting HookOutcomes
    /// (verdict + hook_name) in registration order. Filters by policy and
    /// trust (Disabled empty; ManagedOnly/PluginOnly restrict source;
    /// Untrusted skips Project/Local). timeout_ms > 0 runs hooks in parallel
    /// under a single wall-clock deadline — an unfinished hook is abandoned
    /// (thread leaks, acceptable for misconfigured hooks; true interruption
    /// needs WASM fuel, future work). timeout_ms == 0 is sequential.
    pub fn dispatch(&self, ctx: &HookContext) -> Vec<HookOutcome> {
        if self.policy == HookPolicy::Disabled {
            return Vec::new();
        }
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let Some(ids) = inner.by_event.get(&ctx.event) else {
            return Vec::new();
        };
        let mut filtered: Vec<Arc<dyn Hook>> = Vec::new();
        let mut skipped_untrusted: Vec<String> = Vec::new();
        for &id in ids {
            let Some(h) = inner.hooks.get(&id) else {
                // A stale id (removed between register and dispatch) is
                // skipped, not an error — unregister cleans by_event, but a
                // concurrent dispatch may have cloned the list first.
                continue;
            };
            if !policy_allows(&h.source(), &self.policy) {
                continue;
            }
            if !trust_allows(&h.source(), &self.trust) {
                skipped_untrusted.push(h.name().to_string());
                continue;
            }
            filtered.push(Arc::clone(h));
        }
        if !skipped_untrusted.is_empty() {
            // Queue the one-time notice for the caller to surface as a
            // system line. The registry itself has no channel to the user.
            let mut pending = self
                .pending_skip_notice
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if pending.is_none() {
                *pending = Some(skipped_untrusted);
            }
        }

        if filtered.is_empty() {
            return Vec::new();
        }

        // filtered owns the Arcs, so the read lock is already released
        // implicitly at the end of this scope; drop it now before any hook
        // runs so a built-in hook calling register/unregister during
        // evaluate cannot deadlock the non-reentrant RwLock. The parallel
        // path clones into threads, so sequential stays symmetric.
        let cloned = filtered;
        drop(inner);

        // No timeout: sequential dispatch (fast path for in-process hooks).
        if self.timeout_ms == 0 {
            return cloned
                .iter()
                .map(|h| {
                    let name = h.name().to_string();
                    HookOutcome {
                        result: catch_panic(|| h.evaluate(ctx), &name),
                        hook_name: name,
                    }
                })
                .collect();
        }

        // Parallel dispatch with a single wall-clock deadline.
        //
        // Each hook runs in a detached thread. We collect results via
        // mpsc::recv_timeout, passing the REMAINING time to each receiver
        // (not a fresh timeout). This prevents N hooks from compounding
        // to N*timeout: the total dispatch wall-clock is bounded by
        // timeout_ms regardless of how many hooks hang.
        //
        // A hook that exceeds the deadline is abandoned. Its thread is
        // detached and leaks — the data it references is owned (Arc), so
        // no dangling references. The leaked thread will eventually finish
        // or run forever; for a misconfigured infinite hook this is
        // acceptable because dispatch already returned to the caller.
        let ctx = Arc::new(ctx.clone());
        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms);
        let timeout_ms = self.timeout_ms;

        let receivers: Vec<(mpsc::Receiver<Result<HookVerdict, HookError>>, String)> = cloned
            .iter()
            .map(|hook| {
                let (tx, rx) = mpsc::channel();
                let hook = Arc::clone(hook);
                let ctx = Arc::clone(&ctx);
                let name = hook.name().to_string();
                let thread_name = name.clone();
                std::thread::spawn(move || {
                    let result = catch_panic(|| hook.evaluate(&ctx), &thread_name);
                    drop(tx.send(result));
                });
                (rx, name)
            })
            .collect();

        receivers
            .into_iter()
            .map(|(rx, name)| {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let result = match rx.recv_timeout(remaining) {
                    Ok(result) => result,
                    // The Timeout error flows to the durable HookSignal and
                    // a system line at the append layer, so the user sees
                    // which hook was abandoned.
                    Err(_) => Err(HookError::Timeout {
                        hook_name: name.clone(),
                        limit_ms: timeout_ms,
                    }),
                };
                HookOutcome {
                    hook_name: name,
                    result,
                }
            })
            .collect()
    }

    /// How many hooks are registered.
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .hooks
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// List all registered hooks in registration order for visibility.
    /// Each entry carries the hook name, subscribed events, and config
    /// source.
    pub fn list(&self) -> Vec<HookEntry> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner
            .order
            .iter()
            .filter_map(|id| inner.hooks.get(id))
            .map(|h| HookEntry {
                name: h.name().to_string(),
                events: h.events().to_vec(),
                source: h.source(),
            })
            .collect()
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A registered hook's visible metadata for the /hooks command.
#[derive(Debug, Clone)]
pub struct HookEntry {
    pub name: String,
    pub events: Vec<HookEvent>,
    pub source: HookSource,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::hook::{HookSource, HookVerdict};

    struct StubHook {
        name: String,
        events: Vec<HookEvent>,
        source: HookSource,
    }

    impl Hook for StubHook {
        fn name(&self) -> &str {
            &self.name
        }
        fn events(&self) -> &[HookEvent] {
            &self.events
        }
        fn evaluate(&self, _ctx: &HookContext) -> Result<HookVerdict, HookError> {
            Ok(HookVerdict::Allow)
        }
        fn source(&self) -> HookSource {
            self.source.clone()
        }
    }

    #[test]
    fn test_list_returns_registered_hooks() {
        let reg = HookRegistry::new();
        reg.register(Arc::new(StubHook {
            name: "pre-check".into(),
            events: vec![HookEvent::PreToolUse, HookEvent::PostToolUse],
            source: HookSource::Project,
        }));
        let entries = reg.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "pre-check");
        assert_eq!(
            entries[0].events,
            vec![HookEvent::PreToolUse, HookEvent::PostToolUse]
        );
        assert_eq!(entries[0].source, HookSource::Project);
    }

    #[test]
    fn test_list_empty_registry() {
        let reg = HookRegistry::new();
        assert!(reg.list().is_empty());
    }

    /// The motivating use case for &self register: register on a
    /// shared Arc<HookRegistry> after construction, then dispatch
    /// and verify the hook fires. Proves the interior-mutability
    /// change works end-to-end on a shared reference.
    #[test]
    fn test_register_on_shared_arc() {
        use houyicoder_context::SessionId;

        let reg = Arc::new(HookRegistry::new());
        // Register on the shared Arc — this is the &self path.
        reg.register(Arc::new(StubHook {
            name: "late-hook".into(),
            events: vec![HookEvent::PostToolUse],
            source: HookSource::User,
        }));
        let ctx = HookContext {
            event: HookEvent::PostToolUse,
            payload: HookPayload::PostToolUse {
                tool_name: "bash".into(),
                input: serde_json::json!({}),
                result: ToolResult {
                    output: "{}".into(),
                },
            },
            session: SessionId::new(),
        };
        let outcomes = reg.dispatch(&ctx);
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].hook_name, "late-hook");
    }

    /// unregister removes a hook by its stable HookId: dispatch no longer
    /// fires it, list drops it, and a sibling hook on the same event still
    /// fires (the by_event cleanup filters one id, not the whole event).
    /// An unknown id returns false without touching the registry. The
    /// once:true skill-hook pattern relies on this to drop itself.
    #[test]
    fn test_unregister_removes_hook() {
        use houyicoder_context::SessionId;

        let reg = HookRegistry::new();
        let keep_id = reg.register(Arc::new(StubHook {
            name: "keep".into(),
            events: vec![HookEvent::PostToolUse],
            source: HookSource::User,
        }));
        let drop_id = reg.register(Arc::new(StubHook {
            name: "drop".into(),
            events: vec![HookEvent::PostToolUse],
            source: HookSource::User,
        }));
        // Both fire before unregister.
        let ctx = HookContext {
            event: HookEvent::PostToolUse,
            payload: HookPayload::PostToolUse {
                tool_name: "bash".into(),
                input: serde_json::json!({}),
                result: ToolResult {
                    output: "{}".into(),
                },
            },
            session: SessionId::new(),
        };
        assert_eq!(reg.dispatch(&ctx).len(), 2);
        // Unregister "drop"; "keep" stays. Unknown id returns false.
        assert!(reg.unregister(drop_id), "unregister a live id returns true");
        assert!(
            !reg.unregister(HookId(u64::MAX)),
            "unregister an unknown id returns false"
        );
        let outcomes = reg.dispatch(&ctx);
        assert_eq!(outcomes.len(), 1, "only the kept hook fires");
        assert_eq!(outcomes[0].hook_name, "keep");
        let names: Vec<String> = reg.list().iter().map(|e| e.name.clone()).collect();
        assert_eq!(
            names,
            vec!["keep".to_string()],
            "list drops the unregistered hook"
        );
        // The kept id is still valid (unregister did not over-remove).
        assert!(reg.unregister(keep_id), "the kept id is still live");
        assert_eq!(
            reg.dispatch(&ctx).len(),
            0,
            "no hooks fire after both removed"
        );
    }

    /// HookId is not reused: after unregister, a fresh register mints a
    /// new id, so a stale handle held by a caller cannot accidentally
    /// target a later-registered hook.
    #[test]
    fn test_hook_id_not_reused() {
        let reg = HookRegistry::new();
        let first = reg.register(Arc::new(StubHook {
            name: "first".into(),
            events: vec![HookEvent::PreToolUse],
            source: HookSource::User,
        }));
        assert!(reg.unregister(first));
        let second = reg.register(Arc::new(StubHook {
            name: "second".into(),
            events: vec![HookEvent::PreToolUse],
            source: HookSource::User,
        }));
        assert_ne!(first, second, "a new registration mints a fresh id");
        assert!(
            !reg.unregister(first),
            "the stale handle does not target the new hook"
        );
    }
}
