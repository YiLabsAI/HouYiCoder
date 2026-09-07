//! The concrete SkillRegistry: discovers SKILL.md files at startup via
//! the skill data crate and serves the engine-facing port. Built at the
//! composition root (the single site that constructs concrete impls) and
//! injected into the Skill tool as an Arc<dyn SkillRegistry>. Body
//! preparation delegates to the skill crate's pure functions; this impl
//! only resolves the name, checks the model-invocation gate, and threads
//! the session id into the substitution context.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use houyicoder_api::skill::{
    HookSourceKind, ProjectIdentity, RemoteIdentity, SkillDescriptor, SkillError,
    SkillFamily as ApiSkillFamily, SkillHookSpec, SkillProvenance as ApiSkillProvenance,
    SkillRegistry, SkillScriptRef, SkillSnapshot, SkillSource as ApiSkillSource, SkillUsage,
};
use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_skill::definition::{SkillDefinition, SkillFamily, SkillProvenance, SkillSource};
use houyicoder_skill::disclose::script_gate::detect_skill_scripts;
use houyicoder_skill::lifecycle::{should_swap, watch_roots};
use houyicoder_skill::{discover, invoke};

/// The stable wire label for a discovery family, used for grouping in the
/// skills pane without exposing authority identity or filesystem paths.
fn source_label(source: &SkillSource) -> &'static str {
    match source.family {
        SkillFamily::ClaudeEco => "claude_eco",
        SkillFamily::Agents => "agents",
        SkillFamily::Mcp => "mcp",
        SkillFamily::Houyi => match source.provenance {
            SkillProvenance::Managed => "managed",
            SkillProvenance::UserHome => "user",
            SkillProvenance::Project { .. } => "project",
            SkillProvenance::Remote { .. } => "mcp",
        },
    }
}

fn to_api_source(source: &SkillSource) -> ApiSkillSource {
    let family = match source.family {
        SkillFamily::Houyi => ApiSkillFamily::Houyi,
        SkillFamily::ClaudeEco => ApiSkillFamily::ClaudeEco,
        SkillFamily::Agents => ApiSkillFamily::Agents,
        SkillFamily::Mcp => ApiSkillFamily::Mcp,
    };
    let provenance = match &source.provenance {
        SkillProvenance::Managed => ApiSkillProvenance::Managed,
        SkillProvenance::UserHome => ApiSkillProvenance::UserHome,
        SkillProvenance::Project { root } => {
            ApiSkillProvenance::Project(ProjectIdentity::from_canonical_root(root))
        }
        SkillProvenance::Remote { server } => {
            ApiSkillProvenance::Remote(RemoteIdentity(server.clone()))
        }
    };
    ApiSkillSource::new(family, provenance)
}

/// Map authority provenance to the hook trust level. Directory family does
/// not decide trust: user-home ecosystem hooks are user-owned, project hooks
/// stay behind workspace trust, and remote skills never register commands.
fn skill_source_to_kind(source: &SkillSource) -> Option<HookSourceKind> {
    match source.provenance {
        SkillProvenance::Managed => Some(HookSourceKind::Managed),
        SkillProvenance::UserHome => Some(HookSourceKind::User),
        SkillProvenance::Project { .. } => Some(HookSourceKind::Project),
        SkillProvenance::Remote { .. } => None,
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
        allowed_mach_services: s.allowed_mach_services.clone(),
        allow_app_launch: s.allow_app_launch,
    }
}

/// One immutable discovery snapshot. Skills, descriptors, and hooks are built
/// together and replaced under one lock, so readers cannot observe a torn reload.
pub struct SkillSet {
    skills: Vec<SkillDefinition>,
    descriptors: Vec<SkillDescriptor>,
    hooks: Vec<Vec<SkillHookSpec>>,
}

/// A reload decision and the skill names whose hook registrations changed.
/// changed is empty when swapped is false.
pub struct ReloadOutcome {
    pub swapped: bool,
    pub changed: Vec<String>,
}

/// Filesystem-backed registry with atomic hot-reload and session state that
/// survives discovery snapshot replacement.
pub struct SkillRegistryImpl {
    set: RwLock<SkillSet>,
    usage: Mutex<HashMap<String, SkillUsage>>,
    /// Disabled names remain visible in the skills pane and survive reloads.
    disabled: Mutex<HashSet<String>>,
}

impl SkillRegistryImpl {
    /// Discover project and user-home skills using the process environment.
    pub fn discover(cwd: Option<&Path>) -> Self {
        let home = env::var_os("HOME").map(PathBuf::from);
        Self::discover_with_home(cwd, home.as_deref())
    }

    /// Discover with an explicit home. None skips user-home sources; reserved
    /// command names are rejected before registration.
    pub fn discover_with_home(cwd: Option<&Path>, home: Option<&Path>) -> Self {
        Self {
            set: RwLock::new(build_skillset(cwd, home)),
            usage: Mutex::new(HashMap::new()),
            disabled: Mutex::new(HashSet::new()),
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
        let roots_readable = watch_roots(cwd, home)
            .iter()
            .all(|(p, _)| fs::read_dir(p).is_ok());
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
            entry.last_used_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
        }
    }

    fn usage_for(&self, name: &str) -> SkillUsage {
        self.usage
            .lock()
            .expect("usage lock poisoned")
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    fn set_session_disabled(&self, disabled: HashSet<String>) {
        let count = disabled.len();
        *self.disabled.lock().expect("disabled lock poisoned") = disabled;
        tracing::debug!(count, "session disabled skills set");
    }

    fn source_for(&self, name: &str) -> Option<ApiSkillSource> {
        let set = self.read_set();
        set.skills
            .iter()
            .find(|skill| skill.name == name)
            .map(|skill| to_api_source(&skill.source))
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
            if SlashCommand::is_reserved_skill_name(&s.name) {
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
