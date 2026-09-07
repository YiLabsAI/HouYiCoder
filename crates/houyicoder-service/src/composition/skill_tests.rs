use super::*;
use std::fs;
use std::sync::{Arc, Mutex};

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::skill::grant::SkillGrantStore;
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_core::agent::SkillTool;

fn data_source(family: SkillFamily, provenance: SkillProvenance) -> SkillSource {
    SkillSource::new(family, provenance)
}

fn managed_source() -> SkillSource {
    data_source(SkillFamily::Houyi, SkillProvenance::Managed)
}

fn project_source(family: SkillFamily) -> SkillSource {
    data_source(
        family,
        SkillProvenance::Project {
            root: Path::new("/repo").to_path_buf(),
        },
    )
}

fn user_source(family: SkillFamily) -> SkillSource {
    data_source(family, SkillProvenance::UserHome)
}

fn remote_source() -> SkillSource {
    data_source(
        SkillFamily::Mcp,
        SkillProvenance::Remote {
            server: "server".into(),
        },
    )
}

/// A skill's hook authority follows provenance rather than directory family.
/// User-home ecosystem skills are user hooks, project copies are project
/// hooks, and remote skills never register commands.
#[test]
fn test_skill_source_kind_map() {
    assert_eq!(
        skill_source_to_kind(&managed_source()),
        Some(HookSourceKind::Managed)
    );
    for family in [
        SkillFamily::Houyi,
        SkillFamily::ClaudeEco,
        SkillFamily::Agents,
    ] {
        assert_eq!(
            skill_source_to_kind(&user_source(family)),
            Some(HookSourceKind::User)
        );
        assert_eq!(
            skill_source_to_kind(&project_source(family)),
            Some(HookSourceKind::Project)
        );
    }
    assert_eq!(skill_source_to_kind(&remote_source()), None);
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

fn write_family_skill(root: &Path, family: &str, name: &str) {
    let skill_dir = root.join(family).join("skills").join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: fixture\n---\nbody\n"),
    )
    .unwrap();
}

fn provenance_dir(label: &str) -> std::path::PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "skill-provenance-{label}-{}-{nonce}",
        std::process::id()
    ))
}

#[test]
fn test_home_ecosystem_gets_profile() {
    let home = provenance_dir("home");
    write_family_skill(&home, ".claude", "ego-browser");
    let registry = SkillRegistryImpl::discover_with_home(None, Some(&home));
    let source = registry.source_for("ego-browser").expect("typed source");
    assert_eq!(source.family, ApiSkillFamily::ClaudeEco);
    assert_eq!(source.provenance, ApiSkillProvenance::UserHome);
    let descriptor = registry.find("ego-browser").expect("descriptor");
    let store = SkillGrantStore::with_path(home.join("grants.json"));
    let (mach, app_launch) = store.resolve(
        "ego-browser",
        &source,
        &descriptor.allowed_mach_services,
        descriptor.allow_app_launch,
    );
    assert_eq!(mach, vec!["com.citrolabs.ego.lite.ego-browser"]);
    assert!(app_launch);
    let _cleanup = fs::remove_dir_all(home);
}

struct RecordingSandbox {
    app_launch: Mutex<bool>,
    mach: Mutex<Vec<String>>,
}

impl SandboxSession for RecordingSandbox {
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        unreachable!("the skill invocation does not execute a shell command")
    }

    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(std::env::temp_dir())
    }

    fn set_extra_mach_services(&self, services: &[String]) {
        *self.mach.lock().unwrap() = services.to_vec();
    }

    fn grant_app_launch(&self) {
        *self.app_launch.lock().unwrap() = true;
    }

    fn clear_skill_grants(&self) {
        self.mach.lock().unwrap().clear();
        *self.app_launch.lock().unwrap() = false;
    }
}

#[tokio::test]
async fn test_home_profile_reaches_sandbox() {
    let home = provenance_dir("sandbox");
    write_family_skill(&home, ".claude", "ego-browser");
    let registry: Arc<dyn SkillRegistry> =
        Arc::new(SkillRegistryImpl::discover_with_home(None, Some(&home)));
    let sandbox = Arc::new(RecordingSandbox {
        app_launch: Mutex::new(false),
        mach: Mutex::new(Vec::new()),
    });
    let grants = Arc::new(SkillGrantStore::with_path(home.join("grants.json")));
    let tool = SkillTool::new(registry)
        .with_sandbox(Some(sandbox.clone()))
        .with_skill_grants(Some(grants));
    tool.execute(
        ToolCtx::new("call"),
        serde_json::json!({"skill":"ego-browser"}),
    )
    .await
    .expect("invoke ecosystem skill");
    assert!(*sandbox.app_launch.lock().unwrap());
    assert_eq!(
        *sandbox.mach.lock().unwrap(),
        vec!["com.citrolabs.ego.lite.ego-browser"]
    );
    let _cleanup = fs::remove_dir_all(home);
}

#[test]
fn test_project_ecosystem_skips_profile() {
    let project = provenance_dir("project");
    let home = provenance_dir("shadowed-home");
    fs::create_dir_all(project.join(".git")).unwrap();
    write_family_skill(&project, ".claude", "ego-browser");
    write_family_skill(&home, ".claude", "ego-browser");
    let registry = SkillRegistryImpl::discover_with_home(Some(&project), Some(&home));
    let source = registry.source_for("ego-browser").expect("typed source");
    assert!(matches!(source.provenance, ApiSkillProvenance::Project(_)));
    let descriptor = registry.find("ego-browser").expect("descriptor");
    let store = SkillGrantStore::with_path(home.join("grants.json"));
    let (mach, app_launch) = store.resolve(
        "ego-browser",
        &source,
        &descriptor.allowed_mach_services,
        descriptor.allow_app_launch,
    );
    assert!(mach.is_empty());
    assert!(!app_launch);
    let _project_cleanup = fs::remove_dir_all(project);
    let _home_cleanup = fs::remove_dir_all(home);
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
    let canon_dir = dunce::canonicalize(&skill_dir).unwrap();
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
    let specs = parse_hooks(Some(&raw), &project_source(SkillFamily::Houyi));
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
    assert!(parse_hooks(None, &managed_source()).is_empty());
}

/// MCP skills yield no specs regardless of their hooks block — remote
/// command hooks never register.
#[test]
fn test_parse_hooks_mcp_filtered() {
    let yaml = "PreToolUse:\n  - hooks:\n      - command: ./evil.sh\n";
    let raw = serde_yaml::from_str::<serde_yaml::Value>(yaml).unwrap();
    assert!(
        parse_hooks(Some(&raw), &remote_source()).is_empty(),
        "MCP source produces no specs"
    );
}

/// record_invocation increments invocations + stamps last_used on a
/// successful body preparation (refused=false). Exercised through the
/// real SkillRegistryImpl, not a stub, so the default no-op trait path
/// is not the one tested.
#[test]
fn test_usage_records_invocation() {
    let tmp = std::env::temp_dir().join(format!("skill-usage-inv-{}", std::process::id()));
    write_skill(&tmp, "alpha", "alpha body");
    let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
    assert_eq!(reg.usage_for("alpha").invocations, 0, "starts at zero");
    reg.record_invocation("alpha", false);
    let usage = reg.usage_for("alpha");
    assert_eq!(usage.invocations, 1, "invocation counted");
    assert_eq!(usage.refusals, 0, "no refusal");
    assert!(usage.last_used_secs > 0, "last_used stamped");
    drop(fs::remove_dir_all(&tmp));
}

/// record_invocation increments refusals on a gate refusal (refused=true)
/// and does not stamp last_used (no successful invocation happened).
#[test]
fn test_usage_records_refusal() {
    let tmp = std::env::temp_dir().join(format!("skill-usage-ref-{}", std::process::id()));
    write_skill(&tmp, "beta", "beta body");
    let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
    reg.record_invocation("beta", true);
    let usage = reg.usage_for("beta");
    assert_eq!(usage.invocations, 0, "no invocation");
    assert_eq!(usage.refusals, 1, "refusal counted");
    assert_eq!(usage.last_used_secs, 0, "last_used not stamped on refusal");
    drop(fs::remove_dir_all(&tmp));
}

/// list_with_origin pairs each skill with its accumulated usage stats.
/// An uninvoked skill has default (zero) usage; an invoked skill carries
/// its count.
#[test]
fn test_list_origin_pairs_usage() {
    let tmp = std::env::temp_dir().join(format!("skill-usage-pair-{}", std::process::id()));
    write_skill(&tmp, "alpha", "alpha body");
    let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
    reg.record_invocation("alpha", false);
    reg.record_invocation("alpha", false);
    reg.record_invocation("alpha", true);
    let listing = reg.list_with_origin();
    let alpha = listing
        .iter()
        .find(|s| s.descriptor.name == "alpha")
        .expect("alpha listed");
    assert_eq!(alpha.usage.invocations, 2, "paired invocations");
    assert_eq!(alpha.usage.refusals, 1, "paired refusals");
    drop(fs::remove_dir_all(&tmp));
}

/// Usage survives a reload: editing a skill body does not erase the
/// session's invocation history. The usage field is a sibling of the
/// cached set, not inside it.
#[test]
fn test_usage_survives_reload() {
    let tmp = std::env::temp_dir().join(format!("skill-usage-reload-{}", std::process::id()));
    write_skill(&tmp, "alpha", "alpha body");
    let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
    reg.record_invocation("alpha", false);
    assert_eq!(reg.usage_for("alpha").invocations, 1);
    // Reload: re-discover the same skills. Usage must survive.
    reg.reload(Some(&tmp), None);
    assert_eq!(
        reg.usage_for("alpha").invocations,
        1,
        "usage survives reload"
    );
    drop(fs::remove_dir_all(&tmp));
}

/// A session-disabled skill is excluded from the model listing but stays
/// in list_with_origin (visible in /skills, marked disabled). The disabled
/// set is a sibling of the cached set — reload does not clear it.
#[test]
fn test_disabled_excluded_from_listing() {
    let tmp = std::env::temp_dir().join(format!("skill-disabled-{}", std::process::id()));
    write_skill(&tmp, "alpha", "alpha body");
    write_skill(&tmp, "beta", "beta body");
    let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
    // Both visible before disable
    assert_eq!(reg.list_model_invocable().len(), 2);
    // Disable alpha
    reg.set_session_disabled(["alpha".to_string()].into_iter().collect());
    // alpha excluded from model listing; beta still present
    let names: Vec<String> = reg
        .list_model_invocable()
        .into_iter()
        .map(|d| d.name)
        .collect();
    assert!(!names.contains(&"alpha".to_string()), "disabled excluded");
    assert!(names.contains(&"beta".to_string()), "non-disabled kept");
    // list_with_origin still shows both (visibility surface)
    assert_eq!(reg.list_with_origin().len(), 2);
    drop(fs::remove_dir_all(&tmp));
}

/// A malformed hooks block (not a mapping) is dropped: empty result,
/// no panic (safeParse — the skill still loads).
#[test]
fn test_parse_hooks_malformed_drops() {
    let raw = serde_yaml::Value::String("not a mapping".into());
    assert!(parse_hooks(Some(&raw), &managed_source()).is_empty());
}

/// A malformed event bucket is isolated: the bad event is skipped but a
/// well-formed event in the same block still parses (safeParse does not
/// poison siblings).
#[test]
fn test_parse_hooks_isolates_malformed() {
    let yaml = "PreToolUse:\n  - hooks:\n      - command: ./good.sh\nBroken:\n  - just a string\n";
    let raw = serde_yaml::from_str::<serde_yaml::Value>(yaml).unwrap();
    let specs = parse_hooks(Some(&raw), &managed_source());
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
    let specs = parse_hooks(Some(&raw), &managed_source());
    assert_eq!(specs.len(), 1, "entry without command skipped");
    assert_eq!(specs[0].command, "./good.sh");
}

/// A per-hook timeout key does not panic and the spec is still produced;
/// the timeout is warned + dropped (not yet supported).
#[test]
fn test_parse_hooks_timeout_warns() {
    let yaml = "PreToolUse:\n  - hooks:\n      - command: ./x.sh\n        timeout: 30\n";
    let raw = serde_yaml::from_str::<serde_yaml::Value>(yaml).unwrap();
    let specs = parse_hooks(Some(&raw), &managed_source());
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
        parse_hooks(Some(&raw), &managed_source()).is_empty(),
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

/// Reload picks up a newly added skill: after discovery, writing a new
/// SKILL.md and reloading surfaces the skill in find and the listing.
#[test]
fn test_reload_picks_new_skill() {
    let tmp = std::env::temp_dir().join(format!("skill-reload-new-{}", std::process::id()));
    drop(fs::remove_dir_all(&tmp));
    write_skill(&tmp, "commit", "body");
    let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
    assert!(reg.find("commit").is_some());
    assert!(
        !reg.list_model_invocable()
            .iter()
            .any(|d| d.name == "deploy")
    );
    write_skill(&tmp, "deploy", "deploy body");
    let outcome = reg.reload(Some(&tmp), None);
    assert!(outcome.swapped, "reload swapped");
    assert!(
        outcome.changed.iter().any(|n| n == "deploy"),
        "deploy in changed"
    );
    assert!(reg.find("deploy").is_some(), "deploy found after reload");
    drop(fs::remove_dir_all(&tmp));
}

/// Reload drops a removed skill: deleting a SKILL.md and reloading
/// removes it from the listing, and its name lands in changed.
#[test]
fn test_reload_drops_removed_skill() {
    let tmp = std::env::temp_dir().join(format!("skill-reload-rm-{}", std::process::id()));
    drop(fs::remove_dir_all(&tmp));
    write_skill(&tmp, "commit", "body");
    write_skill(&tmp, "deploy", "deploy body");
    let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
    assert!(reg.find("deploy").is_some());
    fs::remove_dir_all(tmp.join(".houyicoder").join("skills").join("deploy")).unwrap();
    let outcome = reg.reload(Some(&tmp), None);
    assert!(outcome.swapped);
    assert!(
        outcome.changed.iter().any(|n| n == "deploy"),
        "deploy removed in changed"
    );
    assert!(reg.find("deploy").is_none(), "deploy gone after reload");
    assert!(reg.find("commit").is_some(), "commit survives");
    drop(fs::remove_dir_all(&tmp));
}

/// Reload reflects an edited description: the cached descriptor updates
/// after the body is rewritten and reloaded.
#[test]
fn test_reload_reflects_edit() {
    let tmp = std::env::temp_dir().join(format!("skill-reload-edit-{}", std::process::id()));
    drop(fs::remove_dir_all(&tmp));
    write_skill(&tmp, "commit", "v1");
    let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
    let before = reg.find("commit").unwrap();
    let skill_dir = tmp.join(".houyicoder").join("skills").join("commit");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: commit\ndescription: edited-desc\n---\nv2\n",
    )
    .unwrap();
    let outcome = reg.reload(Some(&tmp), None);
    assert!(outcome.swapped);
    let after = reg.find("commit").unwrap();
    assert_ne!(
        after.description, before.description,
        "description refreshed after reload"
    );
    drop(fs::remove_dir_all(&tmp));
}

/// Reload with roots readable and zero skills swaps (legitimate empty:
/// the user deleted the last skill).
#[test]
fn test_reload_roots_readable() {
    let tmp = std::env::temp_dir().join(format!("skill-reload-empty-{}", std::process::id()));
    drop(fs::remove_dir_all(&tmp));
    write_skill(&tmp, "commit", "body");
    let reg = SkillRegistryImpl::discover_with_home(Some(&tmp), None);
    fs::remove_dir_all(tmp.join(".houyicoder").join("skills").join("commit")).unwrap();
    let outcome = reg.reload(Some(&tmp), None);
    assert!(outcome.swapped, "roots readable: empty result swaps");
    assert!(outcome.changed.iter().any(|n| n == "commit"));
    assert!(reg.find("commit").is_none());
    drop(fs::remove_dir_all(&tmp));
}
