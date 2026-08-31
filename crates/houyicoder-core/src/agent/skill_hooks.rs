//! Skill frontmatter hooks: the session-scoped registration path. A skill's
//! parsed hooks become command hooks in the session HookRegistry when the
//! skill is invoked, not at load. A dedup set makes a re-invoke a no-op, and
//! a live workspace-trust ref fail-closes Project and Local sources before
//! registration under an Untrusted workspace.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use houyicoder_api::launcher::ProcessLauncher;
use houyicoder_api::skill::{HookSourceKind, SkillHookSpec, SkillRegistry};
use houyicoder_api::trust::TrustState;

use super::exports::{
    CommandHook, Hook, HookContext, HookError, HookEvent, HookId, HookPayload, HookRegistry,
    HookSource, HookVerdict,
};
use super::parse_event;

/// Shared registration state for both skill-invocation paths: the session
/// hook registry, a live trust ref the server writes after the startup
/// trust prompt, and a dedup set so a re-invoke does not register a second
/// firing copy.
pub struct SkillHookRegistrar {
    hook_reg: Arc<HookRegistry>,
    trust: Arc<RwLock<TrustState>>,
    launcher: Arc<dyn ProcessLauncher>,
    seen: Mutex<HashSet<DedupKey>>,
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
            count += 1;
            tracing::info!(
                skill = %skill_name,
                event = %spec.event,
                source = ?spec.source,
                trust = ?trust_now,
                "registered skill hook"
            );
        }
        count
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
            .is_some_and(|m| !matcher_passes(ctx, m))
        {
            return Ok(HookVerdict::Allow);
        }
        if self
            .if_rule
            .as_ref()
            .is_some_and(|rule| !if_rule_passes(ctx, rule))
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

/// Whether the context's tool name satisfies a matcher pattern. Empty or
/// "*" matches all. A pattern of ascii letters, digits, underscores, and
/// pipes is an exact or pipe-separated list. Anything else is a regex. A
/// non-tool event (no tool name in the payload) never matches.
fn matcher_passes(ctx: &HookContext, matcher: &str) -> bool {
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    let Some(tool) = tool_name(ctx) else {
        return false;
    };
    if matcher
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|')
    {
        if matcher.contains('|') {
            return matcher.split('|').any(|p| p.trim() == tool);
        }
        return matcher == tool;
    }
    match regex::Regex::new(matcher) {
        Ok(re) => re.is_match(tool),
        Err(_) => {
            tracing::warn!(matcher = %matcher, "skill hook matcher is not valid regex");
            false
        }
    }
}

/// Whether the context satisfies a Tool(pattern) if-rule. The tool name
/// must match; a bare Tool (no parens) passes on tool match. A pattern is
/// glob-matched (* and ?) against the tool input's string values as an
/// over-approximation — precise per-field matching is not wired here. A
/// non-tool event never passes.
fn if_rule_passes(ctx: &HookContext, rule: &str) -> bool {
    let (rule_tool, pattern) = parse_if_rule(rule);
    let Some(tool) = tool_name(ctx) else {
        return false;
    };
    if tool != rule_tool {
        return false;
    }
    let Some(pattern) = pattern else {
        return true;
    };
    glob_matches_any(&tool_input(ctx), pattern)
}

/// Split a Tool(pattern) rule into (tool, optional pattern). A bare Tool
/// has no parens.
fn parse_if_rule(rule: &str) -> (&str, Option<&str>) {
    if let Some(open) = rule.find('(') {
        let tool = rule[..open].trim();
        let inner = rule[open + 1..].trim_end_matches(')').trim();
        (tool, Some(inner))
    } else {
        (rule.trim(), None)
    }
}

fn tool_name(ctx: &HookContext) -> Option<&str> {
    match &ctx.payload {
        HookPayload::PreToolUse { tool_name, .. }
        | HookPayload::PostToolUse { tool_name, .. }
        | HookPayload::PostToolUseFailure { tool_name, .. } => Some(tool_name),
        _ => None,
    }
}

fn tool_input(ctx: &HookContext) -> serde_json::Value {
    match &ctx.payload {
        HookPayload::PreToolUse { input, .. } | HookPayload::PostToolUse { input, .. } => {
            input.clone()
        }
        _ => serde_json::Value::Null,
    }
}

/// Glob-match a pattern against any string value in the input JSON. The
/// pattern supports * and ? (translated to regex); other characters are
/// literal. Anchored as a full match.
fn glob_matches_any(input: &serde_json::Value, pattern: &str) -> bool {
    let Some(re) = glob_to_regex(pattern) else {
        return false;
    };
    for s in collect_strings(input) {
        if re.is_match(&s) {
            return true;
        }
    }
    false
}

fn glob_to_regex(pattern: &str) -> Option<regex::Regex> {
    let mut out = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            _ => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    out.push('$');
    regex::Regex::new(&out).ok()
}

/// Collect every string value reachable in the JSON (object values, array
/// elements, nested). Non-string leaves are ignored.
fn collect_strings(value: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_strings_into(value, &mut out);
    out
}

fn collect_strings_into(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(a) => {
            for v in a {
                collect_strings_into(v, out);
            }
        }
        serde_json::Value::Object(o) => {
            for (_, v) in o {
                collect_strings_into(v, out);
            }
        }
        _ => {}
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
