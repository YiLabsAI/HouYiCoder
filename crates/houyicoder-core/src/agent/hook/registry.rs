//! HookRegistry: registration, dispatch, and policy filtering. Split
//! from hook.rs so that file stays under the file-size gate; the
//! registry + list visibility method live here.

use super::*;

pub struct HookRegistry {
    hooks: Vec<Arc<dyn Hook>>,
    by_event: HashMap<HookEvent, Vec<usize>>,
    policy: HookPolicy,
    trust: TrustState,
    /// Per-hook evaluate timeout in milliseconds. Default 5000 (mechanical
    /// rules). 0 disables timeout (sequential dispatch, for tests). A
    /// two-tier timeout (5s mechanical / 60s verify) lands when the Hook
    /// trait gains a kind discriminator or the WASM executor lands.
    timeout_ms: u64,
    /// Hooks skipped for untrusted workspace, pending a one-time notice to
    /// the caller. The skip itself is per-dispatch, but the user only needs
    /// to be told once — a project hook that never fires is a configuration
    /// they wrote and cannot see the effect of. Once drained, later
    /// dispatches skip silently: trust is fixed at construction, so the skip
    /// set cannot change over the registry's lifetime.
    pending_skip_notice: Mutex<Option<Vec<String>>>,
}

impl HookRegistry {
    /// Create an empty registry with the default (all-enabled) policy and
    /// trusted state.
    pub fn new() -> Self {
        Self {
            hooks: Vec::new(),
            by_event: HashMap::new(),
            policy: HookPolicy::default(),
            trust: TrustState::Trusted,
            timeout_ms: 5000,
            pending_skip_notice: Mutex::new(None),
        }
    }

    /// Create an empty registry with an explicit policy (trusted by default).
    pub fn with_policy(policy: HookPolicy) -> Self {
        Self {
            hooks: Vec::new(),
            by_event: HashMap::new(),
            policy,
            trust: TrustState::Trusted,
            timeout_ms: 5000,
            pending_skip_notice: Mutex::new(None),
        }
    }

    /// Create an empty registry with an explicit policy and trust state.
    pub fn with_policy_and_trust(policy: HookPolicy, trust: TrustState) -> Self {
        Self {
            hooks: Vec::new(),
            by_event: HashMap::new(),
            policy,
            trust,
            timeout_ms: 5000,
            pending_skip_notice: Mutex::new(None),
        }
    }

    /// Set the per-hook evaluate timeout. 0 disables timeout (sequential
    /// dispatch). Returns self for chaining.
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

    /// Register a hook. Indexes it under every event it subscribes to for
    /// O(1) lookup at dispatch time.
    pub fn register(&mut self, hook: Arc<dyn Hook>) {
        let idx = self.hooks.len();
        for ev in hook.events() {
            self.by_event.entry(*ev).or_default().push(idx);
        }
        self.hooks.push(hook);
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
        let Some(indices) = self.by_event.get(&ctx.event) else {
            return Vec::new();
        };
        // Partition the registered hooks into the trusted set (the ones
        // that will run) and the skipped set (filtered by policy or
        // trust). The skip is surfaced (not silent) so a user who wired a
        // project-level hook sees why it never fired (untrusted workspace).
        let mut filtered: Vec<&Arc<dyn Hook>> = Vec::new();
        let mut skipped_untrusted: Vec<String> = Vec::new();
        for &i in indices {
            let Some(h) = self.hooks.get(i) else { continue };
            if !policy_allows(&h.source(), &self.policy) {
                continue;
            }
            if !trust_allows(&h.source(), &self.trust) {
                skipped_untrusted.push(h.name().to_string());
                continue;
            }
            filtered.push(h);
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

        // No timeout: sequential dispatch (fast path for in-process hooks).
        if self.timeout_ms == 0 {
            return filtered
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

        let receivers: Vec<(mpsc::Receiver<Result<HookVerdict, HookError>>, String)> = filtered
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
        self.hooks.len()
    }

    /// Whether no hooks are registered.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// List all registered hooks for visibility (/hooks command). Each entry
    /// carries the hook name, subscribed events, and config source.
    pub fn list(&self) -> Vec<HookEntry> {
        self.hooks
            .iter()
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
        let mut reg = HookRegistry::new();
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
}
