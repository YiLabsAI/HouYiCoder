//! The concrete SkillRegistry: discovers SKILL.md files at startup via
//! the skill data crate and serves the engine-facing port. Built at the
//! composition root (the single site that constructs concrete impls) and
//! injected into the Skill tool as an Arc<dyn SkillRegistry>. Body
//! preparation delegates to the skill crate's pure functions; this impl
//! only resolves the name, checks the model-invocation gate, and threads
//! the session id into the substitution context.

use std::path::Path;

use houyicoder_api::skill::{
    HookSourceKind, SkillDescriptor, SkillError, SkillHookSpec, SkillRegistry, SkillScriptRef,
    SkillSnapshot,
};
use houyicoder_core::agent::HookSource;
use houyicoder_skill::definition::{SkillDefinition, SkillSource};
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

/// Map a skill's discovery source to the hook-source level the hook
/// registry gates by. MCP skills are remote and never register command
/// hooks (None); the others map to the trust level they were discovered
/// at. ClaudeEco and Agents are shared-repo paths, grouped with Project
/// so they are skipped under an untrusted project like checked-in hooks.
/// A skill hook built with this source flows through the registry's
/// policy + trust gate the same as a persisted hook.
pub(crate) fn skill_source_to_hook_source(s: &SkillSource) -> Option<HookSource> {
    match s {
        SkillSource::Managed => Some(HookSource::Managed),
        SkillSource::User => Some(HookSource::User),
        SkillSource::Project | SkillSource::ClaudeEco | SkillSource::Agents => {
            Some(HookSource::Project)
        }
        SkillSource::Local => Some(HookSource::Local),
        SkillSource::Mcp => None,
    }
}

/// Map the registry HookSource level to the port-level kind. The two
/// enums mirror each other; the port kind carries no MCP variant (MCP
/// skills produce no hook specs — filtered at parse).
fn hook_source_to_kind(s: HookSource) -> HookSourceKind {
    match s {
        HookSource::Managed => HookSourceKind::Managed,
        HookSource::User => HookSourceKind::User,
        HookSource::Project => HookSourceKind::Project,
        HookSource::Local => HookSourceKind::Local,
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
    let Some(level) = skill_source_to_hook_source(source) else {
        // MCP skills are remote; their command hooks never register.
        return Vec::new();
    };
    let source_kind = hook_source_to_kind(level);
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
pub struct SkillRegistryImpl {
    skills: Vec<SkillDefinition>,
    descriptors: Vec<SkillDescriptor>,
    /// Parsed frontmatter hooks per skill (lockstep with skills/descriptors).
    /// Cached at discovery so hooks_for does not re-parse per invoke. MCP
    /// skills and skills with no/malformed hooks yield empty vectors.
    hooks: Vec<Vec<SkillHookSpec>>,
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
        // Materialize descriptors once: to_descriptor reads each body file
        // for the token estimate. Caching the results here bounds that to
        // one read per skill for the registry's lifetime, so the listing +
        // find paths do not re-read on every call.
        let descriptors = skills.iter().map(to_descriptor).collect();
        // Parse frontmatter hooks once at discovery and cache: hooks_for
        // returns this without re-reading or re-parsing on each invoke.
        // safeParse: malformed hooks yield empty (the skill still loads).
        let hooks = skills
            .iter()
            .map(|s| parse_hooks(s.hooks_raw.as_ref(), &s.source))
            .collect();
        Self {
            skills,
            descriptors,
            hooks,
        }
    }
}

impl SkillRegistry for SkillRegistryImpl {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        self.descriptors
            .iter()
            .filter(|d| !d.disable_model_invocation)
            .cloned()
            .collect()
    }

    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        self.descriptors.iter().find(|d| d.name == name).cloned()
    }

    fn hooks_for(&self, name: &str) -> Vec<SkillHookSpec> {
        // Return the discovery-cached parse (no re-parse per invoke). The
        // lockstep invariant (skills/hooks same order, asserted in
        // list_with_origin) lets a positional lookup mirror find's name
        // resolve. Empty for an unknown skill, an MCP skill, or a skill
        // with no/malformed hooks.
        let Some(i) = self.descriptors.iter().position(|d| d.name == name) else {
            return Vec::new();
        };
        self.hooks.get(i).cloned().unwrap_or_default()
    }

    fn list_with_origin(&self) -> Vec<SkillSnapshot> {
        // Not filtered by disable-model-invocation: this feeds the /skills
        // visibility surface, where a disabled skill must appear marked not
        // invocable. list_model_invocable filters for the model's listing.
        // Descriptors cache parallel to skills (same order, same filter); the
        // assert pins lockstep so a future single-vec mutation fails loudly,
        // not as a silent zip truncation.
        debug_assert_eq!(
            self.skills.len(),
            self.descriptors.len(),
            "skills/descriptors must stay lockstep"
        );
        debug_assert_eq!(
            self.skills.len(),
            self.hooks.len(),
            "skills/hooks must stay lockstep"
        );
        self.skills
            .iter()
            .zip(self.descriptors.iter())
            .map(|(s, d)| SkillSnapshot {
                descriptor: d.clone(),
                origin: source_label(&s.source).into(),
            })
            .collect()
    }

    fn detect_run_scripts(&self, command: &str) -> Vec<SkillScriptRef> {
        use houyicoder_skill::disclose::script_gate::detect_skill_scripts;
        // No file read: the card shows the verifiable path, not a first-line
        // summary (attacker-controlled text framed as authoritative).
        let scan: Vec<(String, SkillSource, &Path)> = self
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
        let def = self
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A skill's discovery source maps to the hook-source level the
    /// registry gates by. MCP never registers (remote, untrusted); the
    /// others map to their trust level. ClaudeEco/Agents group with
    /// Project (shared-repo, skipped under an untrusted project).
    #[test]
    fn test_skill_source_hook_map() {
        assert_eq!(
            skill_source_to_hook_source(&SkillSource::Managed),
            Some(HookSource::Managed)
        );
        assert_eq!(
            skill_source_to_hook_source(&SkillSource::User),
            Some(HookSource::User)
        );
        assert_eq!(
            skill_source_to_hook_source(&SkillSource::Project),
            Some(HookSource::Project)
        );
        assert_eq!(
            skill_source_to_hook_source(&SkillSource::ClaudeEco),
            Some(HookSource::Project),
            "ClaudeEco groups with Project"
        );
        assert_eq!(
            skill_source_to_hook_source(&SkillSource::Agents),
            Some(HookSource::Project),
            "Agents groups with Project"
        );
        assert_eq!(
            skill_source_to_hook_source(&SkillSource::Local),
            Some(HookSource::Local)
        );
        assert_eq!(
            skill_source_to_hook_source(&SkillSource::Mcp),
            None,
            "MCP never registers command hooks"
        );
    }

    fn write_skill(dir: &Path, name: &str, body: &str) {
        let skill_dir = dir.join(".houyicoder").join("skills").join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} skill\n---\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn test_list_filters_disabled() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-list-{}", std::process::id()));
        write_skill(&tmp, "on", "on body");
        let off_dir = tmp.join(".houyicoder").join("skills").join("off");
        fs::create_dir_all(&off_dir).unwrap();
        fs::write(
            off_dir.join("SKILL.md"),
            "---\nname: off\ndescription: off skill\ndisable-model-invocation: true\n---\noff body\n",
        )
        .unwrap();
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        let listing = reg.list_model_invocable();
        let names: Vec<&str> = listing.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"on"), "model-invocable skill listed");
        assert!(
            !names.contains(&"off"),
            "disable-model-invocation skill filtered out"
        );
        drop(fs::remove_dir_all(&tmp));
    }

    /// list_with_origin pairs each model-invocable skill with its discovery
    /// source so the skills pane can group by origin. A project-path skill
    /// under the cwd reports origin "project" — the snake_case label the
    /// pane groups on.
    #[test]
    fn test_list_origin_tags_project() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-origin-{}", std::process::id()));
        write_skill(&tmp, "on", "on body");
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        let snap = reg.list_with_origin();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].descriptor.name, "on");
        assert_eq!(
            snap[0].origin, "project",
            "project-path skill tagged project"
        );
        drop(fs::remove_dir_all(&tmp));
    }

    /// A disable-model-invocation skill must still appear in list_with_origin
    /// (the /skills visibility surface shows it, marked not invocable by the
    /// wire conversion), unlike list_model_invocable which filters it so the
    /// model never sees it. Pins the regression where list_with_origin
    /// filtered disabled skills, making the wire invocable flag always true.
    #[test]
    fn test_list_origin_keeps_disabled() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-dis-origin-{}", std::process::id()));
        write_skill(&tmp, "on", "on body");
        let off_dir = tmp.join(".houyicoder").join("skills").join("off");
        fs::create_dir_all(&off_dir).unwrap();
        fs::write(
            off_dir.join("SKILL.md"),
            "---\nname: off\ndescription: off skill\ndisable-model-invocation: true\n---\noff body\n",
        )
        .unwrap();
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        let snap = reg.list_with_origin();
        // Both skills present — the disabled one is NOT filtered out here
        // (list_model_invocable would return only "on").
        assert_eq!(snap.len(), 2, "disabled skill kept for visibility");
        let off = snap
            .iter()
            .find(|s| s.descriptor.name == "off")
            .expect("off present");
        assert!(
            off.descriptor.disable_model_invocation,
            "disable flag preserved so the wire marks it not invocable"
        );
        assert_eq!(
            reg.list_model_invocable().len(),
            1,
            "model listing filters disabled"
        );
        drop(fs::remove_dir_all(&tmp));
    }

    /// A skill named after a builtin slash command is rejected at
    /// registration so it cannot shadow the builtin at invoke. A project
    /// skill named "compact" must not hijack /compact; the registry drops
    /// it (warned, not silently) and find returns NotFound so the slash
    /// dispatch falls back to the builtin.
    #[test]
    fn test_reserved_name_rejected() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-conflict-{}", std::process::id()));
        write_skill(&tmp, "compact", "hijack body");
        write_skill(&tmp, "commit", "legit body");
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        // The "compact" skill is rejected; "commit" is kept.
        assert!(
            reg.find("compact").is_none(),
            "skill named after a builtin is rejected, not registered"
        );
        assert!(reg.find("commit").is_some(), "non-conflicting skill kept");
        assert_eq!(
            reg.list_model_invocable().len(),
            1,
            "only the non-conflicting skill listed"
        );
        drop(fs::remove_dir_all(&tmp));
    }

    #[test]
    fn test_prepare_body_returns_body() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-body-{}", std::process::id()));
        write_skill(&tmp, "commit", "run git status");
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        let body = reg.prepare_body("commit", None, None).unwrap();
        assert!(body.contains("run git status"), "body present: {body}");
        assert!(
            body.contains("Base directory for this skill"),
            "base-dir header prepended: {body}"
        );
        drop(fs::remove_dir_all(&tmp));
    }

    #[test]
    fn test_prepare_body_substitutes_args() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-args-{}", std::process::id()));
        let skill_dir = tmp.join(".houyicoder").join("skills").join("echo");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: echo\ndescription: echo args\n---\nargs: $ARGUMENTS\n",
        )
        .unwrap();
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        let body = reg.prepare_body("echo", Some("hello world"), None).unwrap();
        assert!(
            body.contains("args: hello world"),
            "args substituted: {body}"
        );
        drop(fs::remove_dir_all(&tmp));
    }

    #[test]
    fn test_unknown_skill_not_found() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-nf-{}", std::process::id()));
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        let err = reg.prepare_body("nope", None, None).unwrap_err();
        match err {
            SkillError::NotFound(n) => assert_eq!(n, "nope"),
            other => panic!("expected NotFound, got {other:?}"),
        }
        drop(fs::remove_dir_all(&tmp));
    }

    #[test]
    fn test_find_exposes_disable_flag() {
        // Gating moved to callers: find returns the descriptor with its
        // disable-model-invocation flag, and the caller (Skill tool) checks
        // it. prepare_body is ungated, so a disabled skill's body is still
        // loadable from the slash path when user-invocable is true.
        let tmp = std::env::temp_dir().join(format!("skill-reg-dis-{}", std::process::id()));
        let off_dir = tmp.join(".houyicoder").join("skills").join("off");
        fs::create_dir_all(&off_dir).unwrap();
        fs::write(
            off_dir.join("SKILL.md"),
            "---\nname: off\ndescription: off\ndisable-model-invocation: true\n---\nbody\n",
        )
        .unwrap();
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        let desc = reg.find("off").expect("find returns the disabled skill");
        assert!(
            desc.disable_model_invocation,
            "the flag the Skill tool gates on is exposed"
        );
        // prepare_body is ungated — the body loads regardless of the flag.
        assert!(
            reg.prepare_body("off", None, None).is_ok(),
            "ungated body loads"
        );
        drop(fs::remove_dir_all(&tmp));
    }

    /// The body token estimate is read once at construction and cached on
    /// the registry. find/listing clone the cached descriptor instead of
    /// re-reading the body file: after construction the body is rewritten
    /// much larger, and the estimate stays at the construction-time value.
    /// A re-reading impl would report the new size; the cache does not.
    #[test]
    fn test_token_estimate_cached() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-tok-{}", std::process::id()));
        write_skill(&tmp, "commit", "run git status");
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        let at_discovery = reg.find("commit").unwrap().body_token_estimate;
        assert!(at_discovery > 0, "estimate computed at discovery");
        let skill_dir = tmp.join(".houyicoder").join("skills").join("commit");
        fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: commit\ndescription: commit skill\n---\n{}\n",
                "x".repeat(4000)
            ),
        )
        .unwrap();
        let after = reg.find("commit").unwrap().body_token_estimate;
        assert_eq!(
            after, at_discovery,
            "cached estimate unchanged after body rewritten"
        );
        drop(fs::remove_dir_all(&tmp));
    }

    #[test]
    fn test_session_id_substituted() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-sid-{}", std::process::id()));
        let skill_dir = tmp.join(".houyicoder").join("skills").join("sid");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: sid\ndescription: sid skill\n---\nsid: ${HOUYI_SESSION_ID}\n",
        )
        .unwrap();
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        let body = reg.prepare_body("sid", None, Some("abc-123")).unwrap();
        assert!(
            body.contains("sid: abc-123"),
            "session id substituted: {body}"
        );
        drop(fs::remove_dir_all(&tmp));
    }

    /// detect_run_scripts returns the skill name + relative script path for a
    /// Bash command that runs a script from a discovered skill's directory. No
    /// file is read — the card shows the verifiable path, not a first-line
    /// summary (attacker-controlled text).
    #[test]
    fn test_detect_run_scripts_summary() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-detect-{}", std::process::id()));
        let skill_dir = tmp.join(".houyicoder").join("skills").join("deploy");
        fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: deploy\ndescription: deploy skill\n---\nbody\n",
        )
        .unwrap();
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        // Discovery stores the canonical skill dir, so the command must name
        // the canonical form for the detector's substring match to fire.
        let canon_dir = std::fs::canonicalize(&skill_dir).unwrap();
        let cmd = format!("python {}/scripts/deploy.py", canon_dir.to_string_lossy());
        let scripts = reg.detect_run_scripts(&cmd);
        assert_eq!(scripts.len(), 1, "one skill script detected: {scripts:?}");
        assert_eq!(scripts[0].skill_name, "deploy");
        assert_eq!(scripts[0].script_rel_path, "scripts/deploy.py");
        // A command that runs no skill script returns empty.
        assert!(
            reg.detect_run_scripts("echo hello && ls /tmp").is_empty(),
            "non-skill command detected nothing"
        );
        drop(fs::remove_dir_all(&tmp));
    }

    /// A well-formed hooks block parses into flat specs carrying the event,
    /// matcher, command, args, once, if-rule, and the source-mapped level.
    #[test]
    fn test_parse_hooks_well_formed() {
        let yaml = r#"
PreToolUse:
  - matcher: "Write|Edit"
    hooks:
      - type: command
        command: ./check.py
        args: ["--strict"]
        once: true
        if: "Write(*)"
  - matcher: "Bash"
    hooks:
      - command: ./audit.sh
  - hooks:
      - command: ./nomatch.sh
"#;
        let raw = serde_yaml::from_str::<serde_yaml::Value>(yaml).unwrap();
        let specs = parse_hooks(Some(&raw), &SkillSource::Project);
        assert_eq!(specs.len(), 3, "three hook entries: {specs:?}");
        let first = &specs[0];
        assert_eq!(first.event, "PreToolUse");
        assert_eq!(first.matcher.as_deref(), Some("Write|Edit"));
        assert_eq!(first.command, "./check.py");
        assert_eq!(first.args, &["--strict".to_string()]);
        assert!(first.once);
        assert_eq!(first.if_rule.as_deref(), Some("Write(*)"));
        assert_eq!(first.source, HookSourceKind::Project);
        let second = &specs[1];
        assert_eq!(second.matcher.as_deref(), Some("Bash"));
        assert!(!second.once);
        assert_eq!(second.command, "./audit.sh");
        // Third bucket has no matcher: None fires for every tool.
        assert_eq!(specs[2].matcher, None);
        assert_eq!(specs[2].command, "./nomatch.sh");
    }

    /// No hooks block yields no specs (the skill still loads, no hooks fire).
    #[test]
    fn test_parse_hooks_none_empty() {
        assert!(parse_hooks(None, &SkillSource::Managed).is_empty());
    }

    /// MCP skills yield no specs regardless of their hooks block — remote
    /// command hooks never register.
    #[test]
    fn test_parse_hooks_mcp_filtered() {
        let yaml = "PreToolUse:\n  - hooks:\n      - command: ./evil.sh\n";
        let raw = serde_yaml::from_str::<serde_yaml::Value>(yaml).unwrap();
        assert!(
            parse_hooks(Some(&raw), &SkillSource::Mcp).is_empty(),
            "MCP source produces no specs"
        );
    }

    /// A malformed hooks block (not a mapping) is dropped: empty result,
    /// no panic (safeParse — the skill still loads).
    #[test]
    fn test_parse_hooks_malformed_drops() {
        let raw = serde_yaml::Value::String("not a mapping".into());
        assert!(parse_hooks(Some(&raw), &SkillSource::Managed).is_empty());
    }

    /// A malformed event bucket is isolated: the bad event is skipped but a
    /// well-formed event in the same block still parses (safeParse does not
    /// poison siblings).
    #[test]
    fn test_parse_hooks_isolates_malformed() {
        let yaml =
            "PreToolUse:\n  - hooks:\n      - command: ./good.sh\nBroken:\n  - just a string\n";
        let raw = serde_yaml::from_str::<serde_yaml::Value>(yaml).unwrap();
        let specs = parse_hooks(Some(&raw), &SkillSource::Managed);
        assert_eq!(
            specs.len(),
            1,
            "well-formed event survives, malformed event skipped"
        );
        assert_eq!(specs[0].event, "PreToolUse");
    }

    /// A hook entry without a command is skipped (no command to spawn); a
    /// sibling entry with a command still parses.
    #[test]
    fn test_parse_hooks_missing_command() {
        let yaml = "PreToolUse:\n  - hooks:\n      - command: ./good.sh\n      - type: command\n";
        let raw = serde_yaml::from_str::<serde_yaml::Value>(yaml).unwrap();
        let specs = parse_hooks(Some(&raw), &SkillSource::Managed);
        assert_eq!(specs.len(), 1, "entry without command skipped");
        assert_eq!(specs[0].command, "./good.sh");
    }

    /// A per-hook timeout key does not panic and the spec is still produced;
    /// the timeout is warned + dropped (not yet supported).
    #[test]
    fn test_parse_hooks_timeout_warns() {
        let yaml = "PreToolUse:\n  - hooks:\n      - command: ./x.sh\n        timeout: 30\n";
        let raw = serde_yaml::from_str::<serde_yaml::Value>(yaml).unwrap();
        let specs = parse_hooks(Some(&raw), &SkillSource::Managed);
        assert_eq!(specs.len(), 1, "spec produced despite timeout key");
        assert_eq!(specs[0].command, "./x.sh");
    }

    /// A non-command hook type is skipped (only command hooks supported);
    /// honest skip, not a silent fire-as-command.
    #[test]
    fn test_parse_hooks_skips_noncommand() {
        let yaml = "PreToolUse:\n  - hooks:\n      - type: prompt\n        command: ./p.sh\n";
        let raw = serde_yaml::from_str::<serde_yaml::Value>(yaml).unwrap();
        assert!(
            parse_hooks(Some(&raw), &SkillSource::Managed).is_empty(),
            "non-command type skipped"
        );
    }

    /// hooks_for returns the discovery-cached parse for a named skill, and
    /// empty for an unknown name — no re-parse per invoke.
    #[test]
    fn test_hooks_for_named_skill() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-hooks-{}", std::process::id()));
        let skill_dir = tmp.join(".houyicoder").join("skills").join("guarded");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: guarded\ndescription: guarded skill\nhooks:\n  PreToolUse:\n    - hooks:\n        - command: ./check.sh\n---\nbody\n",
        )
        .unwrap();
        let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
        let specs = reg.hooks_for("guarded");
        assert_eq!(specs.len(), 1, "cached parse returned for named skill");
        assert_eq!(specs[0].event, "PreToolUse");
        assert!(reg.hooks_for("unknown").is_empty(), "unknown skill empty");
        drop(fs::remove_dir_all(&tmp));
    }
}
