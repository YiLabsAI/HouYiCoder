use super::*;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::skill::{SkillDescriptor, SkillError, SkillRegistry, SkillSnapshot};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// An in-memory registry for tests: stores prepared bodies by name.
/// Every inserted skill defaults to the "managed" origin (trusted) so
/// its body passes through raw; insert_with_origin overrides to test
/// untrusted framing.
struct InMemoryRegistry {
    bodies: HashMap<String, String>,
    model_invocable: HashMap<String, bool>,
    allowed_tools: HashMap<String, Vec<String>>,
    origins: HashMap<String, String>,
}

impl InMemoryRegistry {
    fn new() -> Self {
        Self {
            bodies: HashMap::new(),
            model_invocable: HashMap::new(),
            allowed_tools: HashMap::new(),
            origins: HashMap::new(),
        }
    }

    fn insert(mut self, name: &str, body: &str, model_invocable: bool) -> Self {
        self.bodies.insert(name.to_string(), body.to_string());
        self.model_invocable
            .insert(name.to_string(), model_invocable);
        self.origins.insert(name.to_string(), "managed".into());
        self
    }

    fn insert_with_tools(
        mut self,
        name: &str,
        body: &str,
        model_invocable: bool,
        tools: Vec<String>,
    ) -> Self {
        self.bodies.insert(name.to_string(), body.to_string());
        self.model_invocable
            .insert(name.to_string(), model_invocable);
        self.allowed_tools.insert(name.to_string(), tools);
        self.origins.insert(name.to_string(), "managed".into());
        self
    }

    /// Insert a skill with an explicit discovery origin, to drive the
    /// trust determination (managed/user trusted, anything else
    /// untrusted + framed).
    fn insert_with_origin(
        mut self,
        name: &str,
        body: &str,
        model_invocable: bool,
        origin: &str,
    ) -> Self {
        self.bodies.insert(name.to_string(), body.to_string());
        self.model_invocable
            .insert(name.to_string(), model_invocable);
        self.origins.insert(name.to_string(), origin.into());
        self
    }
}

impl SkillRegistry for InMemoryRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        self.bodies
            .keys()
            .filter(|n| *self.model_invocable.get(*n).unwrap_or(&true))
            .map(|n| SkillDescriptor {
                name: n.clone(),
                description: format!("desc for {n}"),
                when_to_use: None,
                argument_hint: None,
                disable_model_invocation: !*self.model_invocable.get(n).unwrap_or(&true),
                user_invocable: true,
                body_token_estimate: 0,
                allowed_tools: self.allowed_tools.get(n).cloned().unwrap_or_default(),
                allowed_mach_services: Vec::new(),
                allow_app_launch: false,
            })
            .collect()
    }

    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        self.bodies.get(name).map(|_| SkillDescriptor {
            name: name.to_string(),
            description: format!("desc for {name}"),
            when_to_use: None,
            argument_hint: None,
            disable_model_invocation: !*self.model_invocable.get(name).unwrap_or(&true),
            user_invocable: true,
            body_token_estimate: 0,
            allowed_tools: self.allowed_tools.get(name).cloned().unwrap_or_default(),
            allowed_mach_services: Vec::new(),
            allow_app_launch: false,
        })
    }

    fn prepare_body(
        &self,
        name: &str,
        _args: Option<&str>,
        _session_id: Option<&str>,
    ) -> Result<String, SkillError> {
        // Ungated: the Skill tool gates via find; this returns the body.
        self.bodies
            .get(name)
            .cloned()
            .ok_or_else(|| SkillError::NotFound(name.to_string()))
    }

    fn list_with_origin(&self) -> Vec<SkillSnapshot> {
        self.bodies
            .keys()
            .map(|n| SkillSnapshot {
                descriptor: self.find(n).expect("find mirrors insert"),
                origin: self
                    .origins
                    .get(n)
                    .cloned()
                    .unwrap_or_else(|| "managed".into()),
                usage: Default::default(),
            })
            .collect()
    }
}

fn ctx() -> ToolCtx {
    ToolCtx::new("call_1")
}

#[tokio::test]
async fn test_known_skill_returns_body() {
    let reg = Arc::new(InMemoryRegistry::new().insert("commit", "run git status", true));
    let tool = SkillTool::new(reg);
    let out = tool
        .execute(ctx(), json!({"skill": "commit"}))
        .await
        .unwrap();
    assert_eq!(out["skill"], "commit");
    assert_eq!(out["result"], "run git status");
}

/// A trusted-origin skill (managed/user) returns its body raw: no
/// framing wrapper, so the model reads the directive as trusted. This
/// is the Skill-tool-path half of framing not differing by path.
#[tokio::test]
async fn test_trusted_origin_body_unframed() {
    let reg = Arc::new(InMemoryRegistry::new().insert_with_origin(
        "commit",
        "run git status",
        true,
        "managed",
    ));
    let tool = SkillTool::new(reg);
    let out = tool
        .execute(ctx(), json!({"skill": "commit"}))
        .await
        .unwrap();
    let result = out["result"].as_str().expect("result is a string");
    assert_eq!(result, "run git status", "managed-origin body is raw");
    assert!(
        !result.contains("untrusted_skill"),
        "no framing for a trusted origin: {result}"
    );
}

/// An untrusted-origin skill (project/claude-eco/agents/mcp/local)
/// returns its body framed as data: the framing note + the wrapper tag
/// carry the skill name + the body stays inside. A non-managed/user
/// source body is framed, the same framing the slash path applies, so
/// the two invocation paths do not differ.
#[tokio::test]
async fn test_untrusted_origin_body_framed() {
    let reg = Arc::new(InMemoryRegistry::new().insert_with_origin(
        "evil",
        "do bad things",
        true,
        "project",
    ));
    let tool = SkillTool::new(reg);
    let out = tool.execute(ctx(), json!({"skill": "evil"})).await.unwrap();
    let result = out["result"].as_str().expect("result is a string");
    assert!(
        result.contains("unverified data"),
        "framing note present for untrusted origin: {result}"
    );
    assert!(
        result.contains("<untrusted_skill name=\"evil\">"),
        "wrapper carries the skill name: {result}"
    );
    assert!(result.contains("do bad things"), "body present: {result}");
    assert!(
        result.contains("</untrusted_skill>"),
        "wrapper closes: {result}"
    );
}

#[tokio::test]
async fn test_args_reach_registry() {
    // The stub body does not echo args, but the call succeeding
    // proves the args field parsed and reached prepare_body without
    // a schema rejection.
    let reg = Arc::new(InMemoryRegistry::new().insert("commit", "body", true));
    let tool = SkillTool::new(reg);
    let out = tool
        .execute(ctx(), json!({"skill": "commit", "args": "fix typo"}))
        .await
        .unwrap();
    assert_eq!(out["skill"], "commit");
}

#[tokio::test]
async fn test_unknown_skill_errors() {
    let reg = Arc::new(InMemoryRegistry::new());
    let tool = SkillTool::new(reg);
    let err = tool
        .execute(ctx(), json!({"skill": "nope"}))
        .await
        .unwrap_err();
    match err {
        ToolError::Failed(m) => assert!(m.contains("not found"), "{m}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn test_disabled_skill_errors() {
    let reg = Arc::new(InMemoryRegistry::new().insert("secret", "body", false));
    let tool = SkillTool::new(reg);
    let err = tool
        .execute(ctx(), json!({"skill": "secret"}))
        .await
        .unwrap_err();
    match err {
        ToolError::Failed(m) => assert!(m.contains("disabled"), "{m}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn test_missing_skill_arg_errors() {
    let reg = Arc::new(InMemoryRegistry::new());
    let tool = SkillTool::new(reg);
    let err = tool.execute(ctx(), json!({})).await.unwrap_err();
    match err {
        ToolError::InvalidInput(m) => assert!(m.contains("skill"), "{m}"),
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[test]
fn test_body_load_to_io() {
    // A body-read failure surfaces as an I/O tool error, distinct from
    // a wrong name (Failed). Reuses the SkillError Display text so the
    // model sees the cause, not a bare io string.
    let err = skill_error_to_tool_error(SkillError::BodyLoad("permission denied".into()));
    match err {
        ToolError::Io(m) => assert!(m.contains("permission denied"), "{m}"),
        other => panic!("expected Io, got {other:?}"),
    }
}

#[test]
fn test_not_found_uses_display() {
    // The mapping reuses SkillError Display, so the NotFound message
    // the model sees is the Display text, not a separately-maintained
    // string.
    let err = skill_error_to_tool_error(SkillError::NotFound("commit".into()));
    match err {
        ToolError::Failed(m) => assert!(m.contains("not found") && m.contains("commit"), "{m}"),
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// Read-only + non-destructive + approval-free: the tool loads text
/// and mutates no external state, so the loop never gates it.
#[test]
fn test_flags_are_read_only() {
    let reg = Arc::new(InMemoryRegistry::new());
    let tool = SkillTool::new(reg);
    assert!(tool.is_read_only());
    assert!(!tool.is_destructive());
    assert!(!tool.requires_approval());
}

/// Safe-property allowlist: a skill with non-empty allowed_tools asks
/// (it grants permission-bearing tools to the session); a skill with
/// only safe properties is auto-allowed; an unknown skill does not ask
/// (execute fails with NotFound, a clearer signal).
#[test]
fn test_safe_property_allowlist() {
    let reg = InMemoryRegistry::new()
        .insert_with_tools("dangerous", "body", true, vec!["Bash".to_string()])
        .insert("safe", "body", true);
    let tool = SkillTool::new(Arc::new(reg));
    assert!(
        tool.requires_approval_for(&json!({"skill":"dangerous"})),
        "skill with allowed_tools asks"
    );
    assert!(
        !tool.requires_approval_for(&json!({"skill":"safe"})),
        "skill without allowed_tools is auto-allowed"
    );
    assert!(
        !tool.requires_approval_for(&json!({"skill":"unknown"})),
        "unknown skill does not ask (NotFound is clearer)"
    );
}

/// A registry carrying one paths-gated skill.
struct PathsRegistry;
impl SkillRegistry for PathsRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        vec![SkillDescriptor {
            name: "gated".to_string(),
            description: "d".into(),
            when_to_use: None,
            argument_hint: None,
            disable_model_invocation: false,
            user_invocable: true,
            body_token_estimate: 0,
            allowed_tools: Vec::new(),
            allowed_mach_services: Vec::new(),
            allow_app_launch: false,
        }]
    }
    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        (name == "gated").then(|| SkillDescriptor {
            name: "gated".to_string(),
            description: "d".into(),
            when_to_use: None,
            argument_hint: None,
            disable_model_invocation: false,
            user_invocable: true,
            body_token_estimate: 0,
            allowed_tools: Vec::new(),
            allowed_mach_services: Vec::new(),
            allow_app_launch: false,
        })
    }
    fn prepare_body(
        &self,
        _name: &str,
        _args: Option<&str>,
        _session_id: Option<&str>,
    ) -> Result<String, SkillError> {
        Ok("body".into())
    }
    fn paths_for(&self, name: &str) -> Vec<String> {
        if name == "gated" {
            vec!["src".to_string()]
        } else {
            Vec::new()
        }
    }
}

/// A stub activator with a fixed active set.
struct StubActivator {
    active: Mutex<HashSet<String>>,
}
impl StubActivator {
    fn new_empty() -> Self {
        Self {
            active: Mutex::new(HashSet::new()),
        }
    }
    fn with_active(name: &str) -> Self {
        let s = Self::new_empty();
        s.active.lock().unwrap().insert(name.to_string());
        s
    }
}
impl crate::agent::conditional_activation::ConditionalSkillActivator for StubActivator {
    fn activate_for_paths(&self, _file_paths: &[String]) -> Vec<String> {
        Vec::new()
    }
    fn is_active(&self, name: &str) -> bool {
        self.active.lock().unwrap().contains(name)
    }
}

/// A conditional skill refuses until activated; the message names the
/// paths so the model knows what to touch.
#[tokio::test]
async fn test_conditional_refuses_until_active() {
    let reg = Arc::new(PathsRegistry);
    let tool = SkillTool::new(reg).with_activator(Some(Arc::new(StubActivator::new_empty())));
    let err = tool
        .execute(ctx(), json!({"skill": "gated"}))
        .await
        .unwrap_err();
    match err {
        ToolError::Failed(m) => {
            assert!(m.contains("conditional"), "{m}");
            assert!(m.contains("src"), "message names paths: {m}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn test_conditional_passes_when_active() {
    let reg = Arc::new(PathsRegistry);
    let tool =
        SkillTool::new(reg).with_activator(Some(Arc::new(StubActivator::with_active("gated"))));
    let out = tool
        .execute(ctx(), json!({"skill": "gated"}))
        .await
        .unwrap();
    assert_eq!(out["skill"], "gated");
}

/// A sandbox session that records the entitlement grants a skill makes.
struct RecordingSession {
    app_launch: Mutex<Option<bool>>,
    mach: Mutex<Vec<String>>,
}
impl houyicoder_api::sandbox::SandboxSession for RecordingSession {
    fn exec_with_config(
        &self,
        _: &str,
        _: houyicoder_context::ExecConfig,
    ) -> PFut<'_, Result<houyicoder_context::ExecResult, houyicoder_context::SandboxError>> {
        Box::pin(async { Err(houyicoder_context::SandboxError::Unsupported("test".into())) })
    }
    fn read_file(
        &self,
        _: &str,
        _: usize,
    ) -> PFut<'_, Result<Vec<u8>, houyicoder_context::SandboxError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn write_file(
        &self,
        _: &str,
        _: Vec<u8>,
    ) -> PFut<'_, Result<(), houyicoder_context::SandboxError>> {
        Box::pin(async { Ok(()) })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(PathBuf::from("/"))
    }
    fn grant_app_launch(&self) {
        *self.app_launch.lock().unwrap() = Some(true);
    }
    fn set_extra_mach_services(&self, services: &[String]) {
        let mut m = self.mach.lock().unwrap();
        m.clear();
        m.extend_from_slice(services);
    }
}

/// A registry whose skill declares app-launch + a mach service, so the
/// grant wiring is exercised end-to-end through SkillTool::execute.
struct AppLaunchRegistry;
impl SkillRegistry for AppLaunchRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        Vec::new()
    }
    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        (name == "launcher").then(|| SkillDescriptor {
            name: "launcher".to_string(),
            description: "d".into(),
            when_to_use: None,
            argument_hint: None,
            disable_model_invocation: false,
            user_invocable: true,
            body_token_estimate: 0,
            allowed_tools: Vec::new(),
            allowed_mach_services: vec!["ego.mojom.EgoCliBootstrap".into()],
            allow_app_launch: true,
        })
    }
    fn list_with_origin(&self) -> Vec<houyicoder_api::skill::SkillSnapshot> {
        vec![houyicoder_api::skill::SkillSnapshot {
            descriptor: self.find("launcher").unwrap(),
            origin: "managed".into(),
            usage: Default::default(),
        }]
    }
    fn prepare_body(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<String, SkillError> {
        Ok("body".into())
    }
}

/// Same descriptor as AppLaunchRegistry but discovered at a project origin
/// (a SKILL.md checked into a repo). Frontmatter entitlements and the
/// compiled profile must be skipped for an untrusted origin, so execute
/// grants neither app launch nor mach services.
struct ProjectOriginRegistry;
impl SkillRegistry for ProjectOriginRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        Vec::new()
    }
    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        AppLaunchRegistry.find(name)
    }
    fn list_with_origin(&self) -> Vec<houyicoder_api::skill::SkillSnapshot> {
        vec![houyicoder_api::skill::SkillSnapshot {
            descriptor: self.find("launcher").unwrap(),
            origin: "project".into(),
            usage: Default::default(),
        }]
    }
    fn prepare_body(
        &self,
        _: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<String, SkillError> {
        Ok("body".into())
    }
}

#[tokio::test]
async fn test_skill_grants_sandbox_entitlements() {
    let session = Arc::new(RecordingSession {
        app_launch: Mutex::new(None),
        mach: Mutex::new(Vec::new()),
    });
    let tool = SkillTool::new(Arc::new(AppLaunchRegistry)).with_sandbox(Some(session.clone()));
    tool.execute(ctx(), json!({"skill": "launcher"}))
        .await
        .expect("execute");
    let app_launch = *session.app_launch.lock().unwrap();
    assert_eq!(
        app_launch,
        Some(true),
        "app-launch grant wired through execute"
    );
    let mach = session.mach.lock().unwrap().clone();
    assert_eq!(
        mach,
        vec!["ego.mojom.EgoCliBootstrap".to_string()],
        "extra mach services wired through execute"
    );
    // The trait's other methods are stubs; exercise them so the mock's
    // full surface is covered (the grant path does not call them).
    let _r = session
        .exec_with_config("x", houyicoder_context::ExecConfig::default())
        .await;
    let _r = session.read_file("x", 1).await;
    let _r = session.write_file("x", Vec::new()).await;
    assert_eq!(session.workspace_root().as_os_str(), "/");
}

/// A project-origin skill declaring app launch + a mach service must
/// receive neither: the trust gate skips frontmatter and the compiled
/// profile, and the grant store is empty, so resolve returns nothing.
/// Pins the wiring at the call site so an untrusted-origin skill cannot
/// install entitlements by declaring them.
#[tokio::test]
async fn test_untrusted_origin_grants_nothing() {
    let session = Arc::new(RecordingSession {
        app_launch: Mutex::new(None),
        mach: Mutex::new(Vec::new()),
    });
    let tool = SkillTool::new(Arc::new(ProjectOriginRegistry)).with_sandbox(Some(session.clone()));
    tool.execute(ctx(), json!({"skill": "launcher"}))
        .await
        .expect("execute");
    assert!(
        session.app_launch.lock().unwrap().is_none(),
        "untrusted origin must not grant app launch"
    );
    assert!(
        session.mach.lock().unwrap().is_empty(),
        "untrusted origin must not set extra mach services"
    );
}

#[test]
fn test_inmemory_list_shape() {
    let reg = InMemoryRegistry::new().insert("commit", "body", true);
    let list = reg.list_model_invocable();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "commit");
    assert!(!list[0].allow_app_launch);
}
