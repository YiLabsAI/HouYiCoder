//! The concrete SkillRegistry: discovers SKILL.md files at startup via
//! the skill data crate and serves the engine-facing port. Built at the
//! composition root (the single site that constructs concrete impls) and
//! injected into the Skill tool as an Arc<dyn SkillRegistry>. Body
//! preparation delegates to the skill crate's pure functions; this impl
//! only resolves the name, checks the model-invocation gate, and threads
//! the session id into the substitution context.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use houyicoder_api::skill::{
    HookSourceKind, SkillDescriptor, SkillError, SkillHookSpec, SkillRegistry, SkillScriptRef,
    SkillSnapshot,
};
use houyicoder_skill::definition::{SkillDefinition, SkillSource};
use houyicoder_skill::lifecycle::should_swap;
use houyicoder_skill::{discover, invoke};

/// The snake_case wire label for a discovery source, used for grouping in
/// the /skills pane. Mirrors SkillSource's serde rename_all so the wire
/// label stays stable if the enum is ever serialized elsewhere.
fn source_label(source: &SkillSource) -> &'static str {
    match source {
        SkillSource::Managed => "managed",
        SkillSource::User => "user",
        SkillSource::Project => "project",
        SkillSource::ClaudeEco => "claude_eco",
        SkillSource::Agents => "agents",
        SkillSource::Mcp => "mcp",
        SkillSource::Local => "local",
    }
}

/// Map a skill's discovery source to the port-level hook-source kind the
/// registry gates by. MCP skills are remote and never register command
/// hooks (None); the others map to the trust level they were discovered
/// at. ClaudeEco and Agents are shared-repo paths, grouped with Project
/// so they are skipped under an untrusted project like checked-in hooks.
/// A skill hook built with this kind flows through the registry's
/// policy + trust gate the same as a persisted hook. Direct mapping: the
/// engine-side registrar maps the kind onward itself, so no detour
/// through the engine's own source enum is needed here.
fn skill_source_to_kind(s: &SkillSource) -> Option<HookSourceKind> {
    match s {
        SkillSource::Managed => Some(HookSourceKind::Managed),
        SkillSource::User => Some(HookSourceKind::User),
        SkillSource::Project | SkillSource::ClaudeEco | SkillSource::Agents => {
            Some(HookSourceKind::Project)
        }
        SkillSource::Local => Some(HookSourceKind::Local),
        SkillSource::Mcp => None,
    }
}

/// One frontmatter hook entry, deserialized. kind is the optional type
/// field (defaults to command); if_rule is the if permission-rule;
/// timeout is captured only to warn (per-hook timeout is not supported
/// yet).
#[derive(serde::Deserialize)]
struct HookEntry {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    once: bool,
    #[serde(rename = "if", default)]
    if_rule: Option<String>,
    #[serde(default)]
    timeout: Option<serde_yaml::Value>,
}

/// One matcher bucket: a tool-name matcher + the hook entries under it.
#[derive(serde::Deserialize)]
struct MatcherBucket {
    #[serde(default)]
    matcher: Option<String>,
    #[serde(default)]
    hooks: Vec<HookEntry>,
}

/// Deep-parse a skill frontmatter hooks block into engine-facing specs.
/// safeParse: a malformed block or bucket is logged and dropped (the
/// skill still loads, only its hooks drop). The structure is event ->
/// list of matcher buckets; each entry flattens into a spec (event,
/// matcher, command, args, once, if-rule, source). MCP skills yield no
/// specs. Unsupported keys (per-hook timeout, non-command hook types)
/// warn rather than silently ignore.
fn parse_hooks(hooks_raw: Option<&serde_yaml::Value>, source: &SkillSource) -> Vec<SkillHookSpec> {
    let Some(source_kind) = skill_source_to_kind(source) else {
        // MCP skills are remote; their command hooks never register.
        return Vec::new();
    };
    let Some(raw) = hooks_raw else {
        return Vec::new();
    };
    let Some(mapping) = raw.as_mapping() else {
        tracing::warn!("malformed hooks block (not a mapping), dropping hooks");
        return Vec::new();
    };
    let mut specs = Vec::new();
    for (event_val, buckets_val) in mapping {
        let Some(event) = event_val.as_str() else {
            continue;
        };
        let buckets: Vec<MatcherBucket> = match serde_yaml::from_value(buckets_val.clone()) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(event, %e, "malformed hook buckets for event, skipping");
                continue;
            }
        };
        for bucket in buckets {
            let matcher = bucket.matcher;
            for entry in bucket.hooks {
                // Only command hooks are supported; other types warn + skip
                // (honest, no silent fire-as-command). None defaults to
                // command, so the skip fires only for an explicit other type.
                if entry.kind.as_deref().is_some_and(|k| k != "command") {
                    tracing::warn!(event, "non-command hook type, skipping");
                    continue;
                }
                let Some(command) = &entry.command else {
                    tracing::warn!(event, "hook entry without command, skipping");
                    continue;
                };
                if entry.timeout.is_some() {
                    tracing::warn!(event, %command, "per-hook timeout not supported, ignoring");
                }
                specs.push(SkillHookSpec {
                    event: event.to_string(),
                    matcher: matcher.clone(),
                    command: command.clone(),
                    args: entry.args,
                    once: entry.once,
                    if_rule: entry.if_rule,
                    source: source_kind,
                });
            }
        }
    }
    specs
}

/// Map a SkillDefinition to a SkillDescriptor for the engine-facing port,
/// dropping the discovery source (the listing path does not group).
fn to_descriptor(s: &SkillDefinition) -> SkillDescriptor {
    SkillDescriptor {
        name: s.name.clone(),
        description: s.description.clone(),
        when_to_use: s.when_to_use.clone(),
        argument_hint: s.argument_hint.clone(),
        disable_model_invocation: s.disable_model_invocation,
        user_invocable: s.user_invocable,
        body_token_estimate: s.body_token_estimate(),
        allowed_tools: s.allowed_tools.clone(),
    }
}

/// A registry backed by filesystem discovery. Scans the configured paths
/// once at construction; the set is fixed for the session (a skill added
/// mid-session surfaces on the next run, mirroring the external tool
/// server contract). Skills are sorted by precedence at discovery time,
/// so a name lookup returns the highest-precedence match. Descriptors are
/// materialized once at construction and cached: the listing, find, and
/// origin paths clone the cached value instead of re-reading every body
/// file per call (the body token estimate is the only field that touches
/// disk, so caching it once bounds the per-call cost to a clone).
/// The cached discovery set: three parallel vectors built together at
/// discovery and swapped atomically on reload, so a reader never sees a
/// torn mix where one skill's name resolves to another's hooks. The
/// lockstep is structural — the vectors are born and replaced together —
/// not a runtime invariant guarded by asserts.
pub struct SkillSet {
    skills: Vec<SkillDefinition>,
    descriptors: Vec<SkillDescriptor>,
    hooks: Vec<Vec<SkillHookSpec>>,
}

/// The result of a reload: whether the new set was swapped in and which
/// skill names changed (added, removed, or hooks spec changed). The driver
/// feeds changed to the hook registrar so only those skills' hooks are
/// invalidated and re-registered. When swapped is false (the empty-set
/// guard held), changed is empty — no swap means no change to act on.
pub struct ReloadOutcome {
    pub swapped: bool,
    pub changed: Vec<String>,
}

/// A registry backed by filesystem discovery, cached behind a single
/// RwLock so a hot reload can swap the whole set atomically without
/// tearing a reader between the name index and the hooks/skills vectors it
/// indexes into. Descriptors and hooks are materialized once at discovery
/// (the body token estimate is the only field that touches disk, so caching
/// it bounds the per-call cost to a clone).
pub struct SkillRegistryImpl {
    set: RwLock<SkillSet>,
    usage: std::sync::Mutex<std::collections::HashMap<String, houyicoder_api::skill::SkillUsage>>,
    /// Session-scoped disabled skill names. Sibling of set: a disabled skill
    /// is filtered from the model listing but stays in list_with_origin
    /// (visible in /skills, marked disabled). Survives reload (reload swaps
    /// set only).
    disabled: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl SkillRegistryImpl {
    /// Discover skills reading the user-level home from the process env.
    /// Production entry point. Delegates to discover_with_home, which
    /// rejects skills whose names collide with builtin slash commands.
    /// Tests use discover_with_home to pass an explicit (or None) home so
    /// they are not coupled to the real home directory of the machine
    /// running the suite.
    pub fn discover(cwd: Option<&Path>) -> Self {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        Self::discover_with_home(cwd, home.as_deref())
    }

    /// Discover skills with an explicit user-level home directory. A None
    /// home skips the user level so the scan covers only managed + project,
    /// which is what a hermetic test wants. A skill whose name collides with
    /// a builtin slash command is rejected at registration (warned, not
    /// silently dropped) so it cannot shadow the builtin at invoke.
    pub fn discover_with_home(cwd: Option<&Path>, home: Option<&Path>) -> Self {
        Self {
            set: RwLock::new(build_skillset(cwd, home)),
            usage: std::sync::Mutex::new(std::collections::HashMap::new()),
            disabled: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Re-discover and swap the cached set in if the result is sound. A
    /// transient read failure (a watch root unreadable and the set
    /// shrinking) keeps the old set rather than wiping armed skills; a
    /// legitimate empty result (roots readable, user deleted the last skill)
    /// swaps. Returns whether the swap happened and which skill names
    /// changed, so the driver can invalidate and re-register only those
    /// skills' hooks. Build runs without the lock (disk IO); only the diff
    /// and swap take the write lock.
    pub fn reload(&self, cwd: Option<&Path>, home: Option<&Path>) -> ReloadOutcome {
        let new = build_skillset(cwd, home);
        let roots_readable = houyicoder_skill::lifecycle::watch_roots(cwd, home)
            .iter()
            .all(|(p, _)| std::fs::read_dir(p).is_ok());
        let mut set = self.write_set();
        let old = &*set;
        let old_len = old.descriptors.len();
        let new_len = new.descriptors.len();
        if !should_swap(old_len, new_len, roots_readable) {
            tracing::warn!(
                old_len,
                new_len,
                roots_readable,
                "reload kept the old set: a watch root unreadable plus a shrink suggests a transient read failure"
            );
            return ReloadOutcome {
                swapped: false,
                changed: Vec::new(),
            };
        }
        let changed = diff_changed(old, &new);
        *set = new;
        ReloadOutcome {
            swapped: true,
            changed,
        }
    }

    fn read_set(&self) -> RwLockReadGuard<'_, SkillSet> {
        self.set.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write_set(&self) -> RwLockWriteGuard<'_, SkillSet> {
        self.set.write().unwrap_or_else(|e| e.into_inner())
    }
}

impl SkillRegistry for SkillRegistryImpl {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        let set = self.read_set();
        let disabled = self.disabled.lock().expect("disabled lock poisoned");
        set.descriptors
            .iter()
            .filter(|d| !d.disable_model_invocation && !disabled.contains(&d.name))
            .cloned()
            .collect()
    }

    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        let set = self.read_set();
        set.descriptors.iter().find(|d| d.name == name).cloned()
    }

    fn hooks_for(&self, name: &str) -> Vec<SkillHookSpec> {
        // Discovery-cached parse (no re-parse per invoke). A positional
        // lookup mirrors find's name resolve. Empty for an unknown skill,
        // an MCP skill, or a skill with no/malformed hooks.
        let set = self.read_set();
        let Some(i) = set.descriptors.iter().position(|d| d.name == name) else {
            return Vec::new();
        };
        set.hooks.get(i).cloned().unwrap_or_default()
    }

    fn paths_for(&self, name: &str) -> Vec<String> {
        // The normalized globs live on the parsed definition. A positional
        // lookup mirrors hooks_for; empty for an unknown skill or one with
        // no paths (unconditional = always visible).
        let set = self.read_set();
        let Some(i) = set.descriptors.iter().position(|d| d.name == name) else {
            return Vec::new();
        };
        set.skills
            .get(i)
            .map(|s| s.paths.clone())
            .unwrap_or_default()
    }

    fn record_invocation(&self, name: &str, refused: bool) {
        let mut usage = self.usage.lock().expect("usage lock poisoned");
        let entry = usage.entry(name.to_string()).or_default();
        if refused {
            entry.refusals += 1;
        } else {
            entry.invocations += 1;
            entry.last_used_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
        }
    }

    fn usage_for(&self, name: &str) -> houyicoder_api::skill::SkillUsage {
        self.usage
            .lock()
            .expect("usage lock poisoned")
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    fn set_session_disabled(&self, disabled: std::collections::HashSet<String>) {
        let count = disabled.len();
        *self.disabled.lock().expect("disabled lock poisoned") = disabled;
        tracing::debug!(count, "session disabled skills set");
    }

    fn list_with_origin(&self) -> Vec<SkillSnapshot> {
        // Not filtered by disable-model-invocation: this feeds the /skills
        // visibility surface, where a disabled skill must appear marked not
        // invocable. list_model_invocable filters for the model's listing.
        let set = self.read_set();
        let usage = self.usage.lock().expect("usage lock poisoned");
        set.skills
            .iter()
            .zip(set.descriptors.iter())
            .map(|(s, d)| SkillSnapshot {
                descriptor: d.clone(),
                origin: source_label(&s.source).into(),
                usage: usage.get(&s.name).cloned().unwrap_or_default(),
            })
            .collect()
    }

    fn detect_run_scripts(&self, command: &str) -> Vec<SkillScriptRef> {
        use houyicoder_skill::disclose::script_gate::detect_skill_scripts;
        // No file read: the card shows the verifiable path, not a first-line
        // summary (attacker-controlled text framed as authoritative).
        let set = self.read_set();
        let scan: Vec<(String, SkillSource, &Path)> = set
            .skills
            .iter()
            .map(|s| (s.name.clone(), s.source.clone(), s.skill_dir.as_path()))
            .collect();
        detect_skill_scripts(command, &scan)
            .into_iter()
            .map(|r| SkillScriptRef {
                skill_name: r.skill_name,
                script_rel_path: r.script_rel_path,
            })
            .collect()
    }

    fn prepare_body(
        &self,
        name: &str,
        args: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<String, SkillError> {
        // Ungated: the caller gates on the invocation flag via find before
        // calling. A model-disabled but user-invocable skill is reachable
        // here from the slash path.
        let set = self.read_set();
        let def = set
            .skills
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| SkillError::NotFound(name.to_string()))?;
        let ctx = invoke::SubstitutionContext {
            skill_dir: Some(def.skill_dir.as_path()),
            session_id,
            plugin_root: None,
        };
        invoke::prepare_body(def, args, &ctx).map_err(|e| SkillError::BodyLoad(e.to_string()))
    }
}

/// Build a discovery set: scan, drop reserved-name collisions, materialize
/// descriptors (one body read for the token estimate), and parse frontmatter
/// hooks once (safeParse — malformed hooks yield empty, the skill still
/// loads). Extracted so both initial discovery and reload build the same way.
fn build_skillset(cwd: Option<&Path>, home: Option<&Path>) -> SkillSet {
    let skills: Vec<SkillDefinition> = discover::discover_skills(cwd, home)
        .into_iter()
        .filter(|s| {
            if houyicoder_protocol::frontend::SlashCommand::is_reserved_skill_name(&s.name) {
                tracing::warn!(
                    name = %s.name,
                    "skill rejected: name collides with a builtin slash command"
                );
                false
            } else {
                true
            }
        })
        .collect();
    let descriptors = skills.iter().map(to_descriptor).collect();
    let hooks = skills
        .iter()
        .map(|s| parse_hooks(s.hooks_raw.as_ref(), &s.source))
        .collect();
    SkillSet {
        skills,
        descriptors,
        hooks,
    }
}

/// Names whose hook status changed across a reload: added (new name),
/// removed (gone name), and common names whose parsed hooks spec differs.
/// The driver feeds this to the registrar so only those skills' hooks are
/// invalidated and re-registered; unchanged skills are not touched (their
/// once-flags and live trust re-evaluations stay intact).
fn diff_changed(old: &SkillSet, new: &SkillSet) -> Vec<String> {
    let old_names: HashSet<&str> = old.descriptors.iter().map(|d| d.name.as_str()).collect();
    let new_names: HashSet<&str> = new.descriptors.iter().map(|d| d.name.as_str()).collect();
    let mut changed: Vec<String> = Vec::new();
    for d in &new.descriptors {
        if !old_names.contains(d.name.as_str()) {
            changed.push(d.name.clone());
        }
    }
    for d in &old.descriptors {
        if !new_names.contains(d.name.as_str()) {
            changed.push(d.name.clone());
        }
    }
    // common name with hooks spec changed
    for (i, d) in old.descriptors.iter().enumerate() {
        if !new_names.contains(d.name.as_str()) {
            continue;
        }
        let old_hooks = old.hooks.get(i).map(|h| h.as_slice()).unwrap_or(&[]);
        let Some(j) = new.descriptors.iter().position(|nd| nd.name == d.name) else {
            continue;
        };
        let new_hooks = new.hooks.get(j).map(|h| h.as_slice()).unwrap_or(&[]);
        if old_hooks != new_hooks {
            changed.push(d.name.clone());
        }
    }
    changed.sort();
    changed.dedup();
    changed
}

#[cfg(test)]
#[path = "skill_tests.rs"]
mod tests;
