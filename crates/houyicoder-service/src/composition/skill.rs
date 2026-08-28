//! The concrete SkillRegistry: discovers SKILL.md files at startup via
//! the skill data crate and serves the engine-facing port. Built at the
//! composition root (the single site that constructs concrete impls) and
//! injected into the Skill tool as an Arc<dyn SkillRegistry>. Body
//! preparation delegates to the skill crate's pure functions; this impl
//! only resolves the name, checks the model-invocation gate, and threads
//! the session id into the substitution context.

use std::path::Path;

use houyicoder_api::skill::{SkillDescriptor, SkillError, SkillRegistry, SkillSnapshot};
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
/// so a name lookup returns the highest-precedence match.
pub struct SkillRegistryImpl {
    skills: Vec<SkillDefinition>,
}

impl SkillRegistryImpl {
    /// Discover skills from the filesystem, anchored at the given cwd
    /// for the project-level walk. The managed and user levels are
    /// scanned regardless of cwd.
    pub fn discover(cwd: Option<&Path>) -> Self {
        Self {
            skills: discover::discover_skills(cwd),
        }
    }
}

impl SkillRegistry for SkillRegistryImpl {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        self.skills
            .iter()
            .filter(|s| !s.disable_model_invocation)
            .map(to_descriptor)
            .collect()
    }

    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        self.skills
            .iter()
            .find(|s| s.name == name)
            .map(to_descriptor)
    }

    fn list_with_origin(&self) -> Vec<SkillSnapshot> {
        // All discovered skills, NOT filtered by disable-model-invocation:
        // this feeds the /skills user-facing visibility surface, where a
        // disabled skill must appear (marked not invocable) so the user can
        // see it is blocked from the model. The model's own per-turn listing
        // uses list_model_invocable, which DOES filter disabled skills so the
        // model never sees or calls them.
        self.skills
            .iter()
            .map(|s| SkillSnapshot {
                descriptor: to_descriptor(s),
                origin: source_label(&s.source).into(),
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
        let reg = SkillRegistryImpl::discover(Some(&tmp));
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
        let reg = SkillRegistryImpl::discover(Some(&tmp));
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
        let reg = SkillRegistryImpl::discover(Some(&tmp));
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

    #[test]
    fn test_prepare_body_returns_body() {
        let tmp = std::env::temp_dir().join(format!("skill-reg-body-{}", std::process::id()));
        write_skill(&tmp, "commit", "run git status");
        let reg = SkillRegistryImpl::discover(Some(&tmp));
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
        let reg = SkillRegistryImpl::discover(Some(&tmp));
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
        let reg = SkillRegistryImpl::discover(Some(&tmp));
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
        let reg = SkillRegistryImpl::discover(Some(&tmp));
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
        let reg = SkillRegistryImpl::discover(Some(&tmp));
        let body = reg.prepare_body("sid", None, Some("abc-123")).unwrap();
        assert!(
            body.contains("sid: abc-123"),
            "session id substituted: {body}"
        );
        drop(fs::remove_dir_all(&tmp));
    }
}
