//! Skill frontmatter hooks: the session-scoped registration path. A skill's
//! parsed hooks become command hooks in the session HookRegistry when the
//! skill is invoked, not at load. A dedup set makes a re-invoke a no-op, and
//! a live workspace-trust ref fail-closes Project and Local sources before
//! registration under an Untrusted workspace.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use houyicoder_api::launcher::ProcessLauncher;
use houyicoder_api::skill::{HookSourceKind, SkillHookSpec, SkillRegistry};
use houyicoder_api::trust::TrustState;

use super::exports::{
    CommandHook, Hook, HookContext, HookError, HookEvent, HookId, HookRegistry, HookSource,
    HookVerdict,
};
use super::hook::filter;
use super::parse_event;

/// Shared registration state for both skill-invocation paths: the session
/// hook registry, a live trust ref the server writes after the startup
/// trust prompt, a dedup set so a re-invoke does not register a second
/// firing copy, and a per-skill id ledger so a hot reload can invalidate
/// and re-register only the skills whose hooks changed.
pub struct SkillHookRegistrar {
    hook_reg: Arc<HookRegistry>,
    trust: Arc<RwLock<TrustState>>,
    launcher: Arc<dyn ProcessLauncher>,
    seen: Mutex<HashSet<DedupKey>>,
    /// skill name -> ids this registrar registered. Drives per-skill
    /// invalidation on reload; only skills invoked this session have an
    /// entry, so a never-invoked skill's hooks are never armed by reload.
    registered: Mutex<HashMap<String, Vec<HookId>>>,
}

/// Identity a registered skill hook is deduped by. Two specs that agree on
/// skill, event, matcher, command, and args are the same hook. Args are part
/// of the key so two hooks that differ only by arguments both register.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DedupKey {
    skill: String,
    event: String,
    matcher: Option<String>,
    command: String,
    args: Vec<String>,
}

impl SkillHookRegistrar {
    /// Build from the session hook registry, a live trust ref, and the
    /// process launcher the command hooks spawn through. The trust ref
    /// starts fail-closed (Untrusted); the server writes the resolved
    /// state back so a Project or Local source invoked later reads it.
    pub fn new(
        hook_reg: Arc<HookRegistry>,
        trust: Arc<RwLock<TrustState>>,
        launcher: Arc<dyn ProcessLauncher>,
    ) -> Self {
        Self {
            hook_reg,
            trust,
            launcher,
            seen: Mutex::new(HashSet::new()),
            registered: Mutex::new(HashMap::new()),
        }
    }

    /// Write the resolved workspace trust. The server calls this once after
    /// the startup trust prompt. Idempotent.
    pub fn set_trust(&self, state: TrustState) {
        let mut t = self.trust.write().unwrap_or_else(|e| e.into_inner());
        *t = state;
    }

    /// Register every parsed hook for the skill as a session-scoped command
    /// hook. Returns the count newly registered. A Project or Local source
    /// under an Untrusted workspace is skipped fail-closed; an unknown event
    /// name is skipped with a warning; a spec already in the seen set is
    /// skipped (the re-invoke no-op). Managed and User sources pass.
    pub fn register(&self, registry: &dyn SkillRegistry, skill_name: &str) -> usize {
        let specs = registry.hooks_for(skill_name);
        if specs.is_empty() {
            return 0;
        }
        let trust_now = self.trust.read().unwrap_or_else(|e| e.into_inner()).clone();
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let mut count = 0;
        let mut fresh_ids: Vec<HookId> = Vec::new();
        for spec in specs {
            let gated = matches!(spec.source, HookSourceKind::Project | HookSourceKind::Local);
            if gated && trust_now == TrustState::Untrusted {
                tracing::info!(
                    skill = %skill_name,
                    event = %spec.event,
                    source = ?spec.source,
                    "skill hook skipped: untrusted workspace blocks project/local source"
                );
                continue;
            }
            let Some(event) = parse_event(&spec.event) else {
                tracing::warn!(
                    skill = %skill_name,
                    event = %spec.event,
                    "skill hook skipped: unknown event name"
                );
                continue;
            };
            let key = DedupKey {
                skill: skill_name.to_string(),
                event: spec.event.clone(),
                matcher: spec.matcher.clone(),
                command: spec.command.clone(),
                args: spec.args.clone(),
            };
            if !seen.insert(key.clone()) {
                tracing::info!(
                    skill = %skill_name,
                    event = %spec.event,
                    "skill hook skipped: already registered this session (dedup)"
                );
                continue;
            }
            let source = map_source(spec.source);
            let hook = SkillCommandHook::new(
                skill_name.to_string(),
                vec![event],
                source,
                &spec,
                Arc::clone(&self.launcher),
                Arc::clone(&self.hook_reg),
            );
            // Keep a concrete Arc so bind_hook_id can reach the same instance
            // the registry holds (both Arcs share the allocation). The dyn
            // coercion happens at the typed let, not in register's arg.
            let concrete = Arc::new(hook);
            let dyn_hook: Arc<dyn Hook> = concrete.clone();
            let id = self.hook_reg.register(dyn_hook);
            concrete.bind_hook_id(id);
            fresh_ids.push(id);
            count += 1;
            tracing::info!(
                skill = %skill_name,
                event = %spec.event,
                source = ?spec.source,
                trust = ?trust_now,
                "registered skill hook"
            );
        }
        // Record the freshly registered ids under the skill so a reload can
        // invalidate and re-register only this skill's hooks. The seen lock is
        // held here; the registered lock is taken after, briefly, to avoid a
        // second long-held lock through the spec loop.
        if !fresh_ids.is_empty() {
            let mut registered = self.registered.lock().unwrap_or_else(|e| e.into_inner());
            registered
                .entry(skill_name.to_string())
                .or_default()
                .extend(fresh_ids);
        }
        count
    }

    /// Invalidate and re-register the hooks of skills whose hook status
    /// changed across a reload. Only skills already in the registered ledger
    /// (invoked this session) are touched: a never-invoked skill — including
    /// one a hostile repository drops in mid-session — is skipped so its hooks
    /// are not armed behind the invoke-time registration gate. For an invoked
    /// skill, the old ids are unregistered, its dedup entries cleared, and the
    /// fresh spec re-registered (which re-evaluates trust against the live
    /// workspace state). A removed skill re-registers nothing (its fresh
    /// hooks_for is empty). unchanged skills are not passed here.
    ///
    /// Lock order: register takes seen then registered. To avoid an AB-BA
    /// deadlock, invalidate never holds registered across a seen acquisition:
    /// the first pass drains the ledger under registered, the second pass
    /// takes seen + re-registers with no registered held.
    pub fn invalidate(&self, changed: &[String], registry: &dyn SkillRegistry) {
        // Pass 1: under registered, pull each invoked skill's old ids out of
        // the ledger. Never-invoked skills are skipped here (the invoke-time
        // gate).
        let to_invalidate: Vec<(String, Vec<HookId>)> = {
            let mut registered = self.registered.lock().unwrap_or_else(|e| e.into_inner());
            let mut out = Vec::new();
            for name in changed {
                if registered.contains_key(name) {
                    let ids = registered.remove(name).unwrap_or_default();
                    out.push((name.clone(), ids));
                }
            }
            out
        }; // registered released here

        // Pass 2: clear dedup entries, unregister old ids, re-register fresh.
        // seen is taken without registered held, matching register's order.
        for (name, old_ids) in &to_invalidate {
            {
                let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
                seen.retain(|k| k.skill != *name);
            }
            for id in old_ids {
                self.hook_reg.unregister(*id);
            }
            tracing::info!(
                skill = %name,
                invalidated = old_ids.len(),
                "skill hooks invalidated for reload"
            );
            // Re-register with the fresh spec (re-evaluates trust). register
            // takes seen then registered; neither is held here.
            self.register(registry, name);
        }
    }
}

fn map_source(kind: HookSourceKind) -> HookSource {
    match kind {
        HookSourceKind::Managed => HookSource::Managed,
        HookSourceKind::User => HookSource::User,
        HookSourceKind::Project => HookSource::Project,
        HookSourceKind::Local => HookSource::Local,
    }
}

/// A hook built from a skill frontmatter spec. The matcher gates by tool
/// name (exact, pipe-separated, or regex); the if-rule gates by tool and
/// input pattern; the wrapped command hook spawns the configured program
/// and parses its verdict. The verdict is narrowed by source: a Project or
/// Local source cannot Inject (downgraded to Observe so a project hook
/// cannot speak with engine authority) and an Ask is tagged with the skill
/// name; Managed and User sources pass through. A once hook self-removes
/// after its first spawn via an AtomicBool race winner; the flag is the sole
/// authority so concurrent dispatches spawn exactly once. A once hook fires
/// once regardless of spawn outcome — a failed spawn does not retry.
pub(crate) struct SkillCommandHook {
    name: String,
    events: Vec<HookEvent>,
    source: HookSource,
    matcher: Option<String>,
    if_rule: Option<String>,
    command: CommandHook,
    once: bool,
    fired: AtomicBool,
    hook_reg: Arc<HookRegistry>,
    hook_id: OnceLock<HookId>,
}

impl SkillCommandHook {
    pub(crate) fn new(
        name: String,
        events: Vec<HookEvent>,
        source: HookSource,
        spec: &SkillHookSpec,
        launcher: Arc<dyn ProcessLauncher>,
        hook_reg: Arc<HookRegistry>,
    ) -> Self {
        let command = CommandHook::new(
            name.clone(),
            events.clone(),
            spec.command.clone(),
            spec.args.clone(),
            launcher,
            source.clone(),
        );
        Self {
            name,
            events,
            source,
            matcher: spec.matcher.clone(),
            if_rule: spec.if_rule.clone(),
            command,
            once: spec.once,
            fired: AtomicBool::new(false),
            hook_reg,
            hook_id: OnceLock::new(),
        }
    }

    /// Bind the registry-assigned id so the hook can self-unregister after a
    /// once fire. Called by the registrar right after registration; a
    /// concurrent dispatch that fires before this is set skips the unregister
    /// (the AtomicBool still blocks a second spawn).
    pub(crate) fn bind_hook_id(&self, id: HookId) {
        let _ = self.hook_id.set(id);
    }
}

impl Hook for SkillCommandHook {
    fn name(&self) -> &str {
        &self.name
    }
    fn events(&self) -> &[HookEvent] {
        &self.events
    }
    fn source(&self) -> HookSource {
        self.source.clone()
    }
    fn evaluate(&self, ctx: &HookContext) -> Result<HookVerdict, HookError> {
        if self
            .matcher
            .as_ref()
            .is_some_and(|m| !filter::matcher_passes(ctx, m))
        {
            return Ok(HookVerdict::Allow);
        }
        if self
            .if_rule
            .as_ref()
            .is_some_and(|rule| !filter::if_rule_passes(ctx, rule))
        {
            return Ok(HookVerdict::Allow);
        }
        // once: only the dispatch that wins the compare_exchange spawns.
        // A lost race returns Allow without spawning; the AtomicBool is the
        // sole authority, so concurrent dispatches spawn exactly once.
        if self.once
            && self
                .fired
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Ok(HookVerdict::Allow);
        }
        let result = self.command.evaluate(ctx);
        // Self-unregister after the once spawn (best-effort cleanup so later
        // dispatches do not call a dead hook; correctness is in the AtomicBool).
        // Dispatch releases the read lock before evaluate, so unregister takes
        // the write lock without deadlocking.
        if self.once
            && let Some(&id) = self.hook_id.get()
        {
            self.hook_reg.unregister(id);
        }
        let verdict = result?;
        Ok(narrow_by_source(verdict, self.source.clone(), &self.name))
    }
}

/// Narrow a verdict by source level. A Project or Local source cannot
/// Inject (the content is downgraded to an Observe so a project hook cannot
/// inject instructions the model reads as engine-authoritative) and an Ask
/// is tagged with the skill name so the user sees which skill is asking.
/// Managed and User sources pass through unchanged.
fn narrow_by_source(verdict: HookVerdict, source: HookSource, name: &str) -> HookVerdict {
    match source {
        HookSource::Project | HookSource::Local => match verdict {
            HookVerdict::Inject(content) => HookVerdict::Observe(content),
            HookVerdict::Ask(msg) => HookVerdict::Ask(format!("[skill {name}] {msg}")),
            other => other,
        },
        _ => verdict,
    }
}

#[cfg(test)]
#[path = "skill_hooks_tests.rs"]
mod tests;
