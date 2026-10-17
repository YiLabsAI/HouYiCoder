//! Tests for this module: which discovered skills one assembly counts, and
//! which of them the rows still cover.

use super::super::*;
use crate::agent::ToolRegistry;
use crate::agent::runner_config::RunnerConfig;
use crate::provider::test_support::FakeProvider;
use houyicoder_api::skill::{
    SkillDescriptor, SkillError, SkillRegistry, SkillSnapshot, SkillUsage,
};
use houyicoder_memory::InMemoryBackend;
use houyicoder_session::SessionStore;
use std::sync::Arc;

fn descriptor(name: &str) -> SkillDescriptor {
    SkillDescriptor {
        name: name.to_string(),
        description: format!("{name} does something"),
        when_to_use: None,
        argument_hint: None,
        disable_model_invocation: false,
        user_invocable: true,
        body_token_estimate: 0,
        allowed_tools: Vec::new(),
        allowed_mach_services: Vec::new(),
        allow_app_launch: false,
    }
}

fn snapshot(name: &str, origin: &str) -> SkillSnapshot {
    SkillSnapshot {
        descriptor: descriptor(name),
        origin: origin.to_string(),
        usage: SkillUsage::default(),
    }
}

/// A registry whose discovery set holds a skill the model may not invoke, the
/// shape a skill the session disabled takes.
struct ManualRegistry {
    discovered: Vec<SkillSnapshot>,
    announced: Vec<SkillDescriptor>,
}

impl SkillRegistry for ManualRegistry {
    fn list_model_invocable(&self) -> Vec<SkillDescriptor> {
        self.announced.clone()
    }

    fn find(&self, name: &str) -> Option<SkillDescriptor> {
        self.discovered
            .iter()
            .find(|snapshot| snapshot.descriptor.name == name)
            .map(|snapshot| snapshot.descriptor.clone())
    }

    fn prepare_body(
        &self,
        _name: &str,
        _args: Option<&str>,
        _session_id: Option<&str>,
    ) -> Result<String, SkillError> {
        Ok(String::new())
    }

    fn list_with_origin(&self) -> Vec<SkillSnapshot> {
        self.discovered.clone()
    }
}

fn runner() -> Runner {
    Runner::new(
        Arc::new(SessionStore::new(Box::new(InMemoryBackend::new()))),
        Arc::new(FakeProvider::text("x")),
        ToolRegistry::new(),
        RunnerConfig::default(),
    )
}

/// One assembly counts exactly the skills the listing carries: a skill
/// discovery found but the registry reports as manual is not in the assembled
/// set, because counting it would report a cost the request never carries. The
/// rows still cover it, marked as the user's to invoke.
#[test]
fn test_assembly_carries_listed_skills() {
    let registry: Arc<dyn SkillRegistry> = Arc::new(ManualRegistry {
        discovered: vec![
            snapshot("commit", "project"),
            snapshot("brainstorm", "claude_eco"),
        ],
        announced: vec![descriptor("commit")],
    });
    let parts = runner().with_skill_registry(registry).assembly_parts();
    let counted: Vec<&str> = parts
        .announced_skills
        .iter()
        .map(|snapshot| snapshot.descriptor.name.as_str())
        .collect();
    assert_eq!(counted, ["commit"], "only the listed skill is counted");
    assert_eq!(
        parts.discovered_skills.len(),
        2,
        "the /context rows still cover discovery"
    );
}
