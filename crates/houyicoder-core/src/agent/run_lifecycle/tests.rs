//! Tests for this module.

use super::*;
use houyicoder_api::sandbox::SandboxSession;
use houyicoder_api::skill::grant::SkillGrantStore;
use houyicoder_api::skill::{
    SkillDescriptor, SkillError, SkillFamily, SkillProvenance, SkillRegistry, SkillSource,
};
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A sandbox session that records entitlement grants and clears.
struct RecordingSession {
    mach: Mutex<Vec<String>>,
    app_launch: Mutex<bool>,
    cleared: Mutex<u32>,
}
impl RecordingSession {
    fn new() -> Self {
        Self {
            mach: Mutex::new(Vec::new()),
            app_launch: Mutex::new(false),
            cleared: Mutex::new(0),
        }
    }
}
impl SandboxSession for RecordingSession {
    fn exec_with_config(
        &self,
        _: &str,
        _: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::Unsupported("test".into())) })
    }
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(PathBuf::from("/"))
    }
    fn set_extra_mach_services(&self, services: &[String]) {
        let mut m = self.mach.lock().unwrap();
        m.clear();
        m.extend_from_slice(services);
    }
    fn grant_app_launch(&self) {
        *self.app_launch.lock().unwrap() = true;
    }
    fn clear_skill_grants(&self) {
        *self.cleared.lock().unwrap() += 1;
        self.mach.lock().unwrap().clear();
        *self.app_launch.lock().unwrap() = false;
    }
}

/// A registry whose skill declares a mach service + app launch.
struct EntitlementRegistry;
impl SkillRegistry for EntitlementRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        Vec::new()
    }
    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        (name == "ego-browser").then(|| SkillDescriptor {
            name: "ego-browser".to_string(),
            description: "d".into(),
            when_to_use: None,
            argument_hint: None,
            disable_model_invocation: false,
            user_invocable: true,
            body_token_estimate: 0,
            allowed_tools: Vec::new(),
            allowed_mach_services: vec!["com.citrolabs.ego.lite.ego-browser".into()],
            allow_app_launch: true,
        })
    }
    fn source_for(&self, name: &str) -> Option<SkillSource> {
        (name == "ego-browser")
            .then(|| SkillSource::new(SkillFamily::Agents, SkillProvenance::UserHome))
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

/// When a skill is active, reapply re-grants its mach services and app
/// launch from the frontmatter/profile, so a later-turn bash command can
/// reach the ego bootstrap.
#[test]
fn test_reapply_carries_active_skill() {
    let session = RecordingSession::new();
    reapply_skill_entitlements_impl(
        Some("ego-browser".into()),
        Some(&session),
        Some(&EntitlementRegistry),
        None,
    );
    let mach = session.mach.lock().unwrap().clone();
    assert_eq!(
        mach,
        vec!["com.citrolabs.ego.lite.ego-browser".to_string()],
        "mach services re-granted for carried skill"
    );
    assert!(
        *session.app_launch.lock().unwrap(),
        "app launch re-granted for carried skill"
    );
}

/// When no skill is active, reapply clears the slate so a prior skill's
/// grants do not leak into an unrelated run.
#[test]
fn test_reapply_clears_without_skill() {
    let session = RecordingSession::new();
    reapply_skill_entitlements_impl(None, Some(&session), Some(&EntitlementRegistry), None);
    assert!(
        session.mach.lock().unwrap().is_empty(),
        "mach cleared when no skill active"
    );
    assert!(
        !*session.app_launch.lock().unwrap(),
        "app launch cleared when no skill active"
    );
    assert!(
        *session.cleared.lock().unwrap() > 0,
        "clear_skill_grants called"
    );
}

/// A grant store with a user-approved service feeds into reapply, so a
/// denial approved in a prior turn takes effect on the next turn.
#[test]
fn test_reapply_picks_up_grant() {
    let session = RecordingSession::new();
    let grants = SkillGrantStore::with_path(temp_dir().join(format!(
        "houyi-reapply-grants-{}-{}.json",
        id(),
        1
    )));
    let source = SkillSource::new(SkillFamily::Agents, SkillProvenance::UserHome);
    grants
        .add_grants(
            &source.grant_subject("ego-browser"),
            vec!["com.test.extra.service".into()],
        )
        .unwrap();
    reapply_skill_entitlements_impl(
        Some("ego-browser".into()),
        Some(&session),
        Some(&EntitlementRegistry),
        Some(&grants),
    );
    let mach = session.mach.lock().unwrap().clone();
    assert!(
        mach.contains(&"com.test.extra.service".to_string()),
        "user-approved grant picked up on reapply: {mach:?}"
    );
    assert!(
        mach.contains(&"com.citrolabs.ego.lite.ego-browser".to_string()),
        "profile service still present: {mach:?}"
    );
}

/// Exercise the stub trait methods so diff-cov sees them. The stub
/// session and registry implement required trait methods that the
/// reapply path does not call; this test touches them once.
#[test]
fn test_stubs_are_callable() {
    let session = RecordingSession::new();
    let _root = session.workspace_root();
    let reg = EntitlementRegistry;
    assert!(reg.list_model_invocable().is_empty());
    assert!(reg.prepare_body("x", None, None).is_ok());
}
