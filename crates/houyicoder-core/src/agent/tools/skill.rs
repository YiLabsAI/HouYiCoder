//! The Skill tool: the model-invocable entry point for loading a skill
//! body on demand. The model calls this tool with a skill name (and
//! optional args) when it decides a skill applies; the tool resolves the
//! name through the SkillRegistry port, prepares the body (argument and
//! variable substitution done by the registry impl), and returns it as
//! the tool result. Progressive disclosure: the listing attachment the
//! model sees each turn carries only descriptions; the full body is
//! loaded here, only when invoked.
//!
//! A large body past the isolation threshold is externalized to the CAS
//! by the agent loop's large-output isolation, so the model sees a
//! preview and can materialize on demand rather than re-reading the
//! whole body each turn.

use std::sync::{Arc, Mutex};

use houyicoder_api::skill::{SkillError, SkillRegistry};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use serde::Deserialize;
use serde_json::{Value, json};

/// The Skill tool. Holds a SkillRegistry port; the concrete registry
/// is constructed at the composition root and injected here. Loading a
/// body is read-only, while optional host-owned integrations update the
/// current sandbox entitlement and invocation metadata.
pub struct SkillTool {
    registry: Arc<dyn SkillRegistry>,
    registrar: Option<Arc<super::super::SkillHookRegistrar>>,
    activator: Option<Arc<dyn super::super::conditional_activation::ConditionalSkillActivator>>,
    sandbox: Option<Arc<dyn houyicoder_api::sandbox::SandboxSession>>,
    skill_grants: Option<Arc<houyicoder_api::skill::grant::SkillGrantStore>>,
    active_skill: Option<Arc<Mutex<Option<String>>>>,
}

impl SkillTool {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self {
            registry,
            registrar: None,
            activator: None,
            sandbox: None,
            skill_grants: None,
            active_skill: None,
        }
    }

    /// Wire the skill-hook registrar so invoking a skill registers its
    /// frontmatter hooks into the session hook registry. Unwired in tests
    /// that do not exercise hooks; the execute path skips registration.
    pub fn with_registrar(mut self, registrar: Arc<super::super::SkillHookRegistrar>) -> Self {
        self.registrar = Some(registrar);
        self
    }

    /// Wire the sandbox session so invoking a skill grants entitlements.
    pub fn with_sandbox(
        mut self,
        sandbox: Option<Arc<dyn houyicoder_api::sandbox::SandboxSession>>,
    ) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Wire the skill grant store so invocation merges granted mach services.
    pub fn with_skill_grants(
        mut self,
        grants: Option<Arc<houyicoder_api::skill::grant::SkillGrantStore>>,
    ) -> Self {
        self.skill_grants = grants;
        self
    }

    /// Wire the shared active-skill cell for post-bash-failure attribution.
    pub fn with_active_skill(mut self, cell: Option<Arc<Mutex<Option<String>>>>) -> Self {
        self.active_skill = cell;
        self
    }

    /// Wire the paths-skill activator so a conditional skill refuses until a
    /// matching file touch activates it. Unwired in tests; the gate then
    /// passes (feature off).
    pub fn with_activator(
        mut self,
        activator: Option<Arc<dyn super::super::conditional_activation::ConditionalSkillActivator>>,
    ) -> Self {
        self.activator = activator;
        self
    }
}

#[derive(Debug, Deserialize)]
struct SkillInput {
    skill: String,
    #[serde(default)]
    args: Option<String>,
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Invoke a skill by name to load its full instructions. \
         Available skills and their descriptions are listed in the \
         system-reminder messages in the conversation. Call this tool \
         only when a skill applies to the current task; do not call it \
         if you already see the skill's instructions in the \
         conversation. Input: {skill: string, args?: string}. Returns \
         the prepared skill body; follow its instructions directly."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "skill": {
                    "type": "string",
                    "description": "The skill name (the directory name, e.g. \"commit\")."
                },
                "args": {
                    "type": "string",
                    "description": "Optional arguments for the skill."
                }
            },
            "required": ["skill"]
        })
    }

    fn execute(&self, ctx: ToolCtx, input: Value) -> PFut<'_, Result<Value, ToolError>> {
        let registry = Arc::clone(&self.registry);
        let registrar = self.registrar.clone();
        let activator = self.activator.clone();
        Box::pin(async move {
            let params: SkillInput = serde_json::from_value(input)
                .map_err(|e| ToolError::InvalidInput(format!("skill: {e}")))?;
            // The session id feeds variable substitution inside the body
            // (skill-dir and session-id tokens). None when the dispatch is
            // not session-bound (a non-interactive run, a test).
            let sid = ctx.session_id.map(|s| s.to_string());
            // Gate on disable-model-invocation (the model cannot call a
            // skill hidden from it). The registry's find returns the flag;
            // the shared prepare_body is ungated so the slash path reaches
            // model-disabled skills when user-invocable.
            let desc = registry
                .find(&params.skill)
                .ok_or_else(|| ToolError::Failed(format!("skill not found: {}", params.skill)))?;
            if desc.disable_model_invocation {
                registry.record_invocation(&params.skill, true);
                return Err(ToolError::Failed(format!(
                    "skill {} is disabled for model invocation",
                    params.skill
                )));
            }
            // Gate on paths: a conditional skill refuses until a file touch
            // activates it. unwrap_or(true) means unwired = feature off =
            // pass, not fail-closed.
            let paths = registry.paths_for(&params.skill);
            if !paths.is_empty()
                && !activator
                    .as_ref()
                    .map(|a| a.is_active(&params.skill))
                    .unwrap_or(true)
            {
                registry.record_invocation(&params.skill, true);
                return Err(ToolError::Failed(format!(
                    "skill {} is conditional; touch a matching file to activate: {}",
                    params.skill,
                    paths.join(", ")
                )));
            }
            let body = match registry.prepare_body(
                &params.skill,
                params.args.as_deref(),
                sid.as_deref(),
            ) {
                Ok(b) => {
                    registry.record_invocation(&params.skill, false);
                    b
                }
                Err(e) => {
                    registry.record_invocation(&params.skill, true);
                    return Err(skill_error_to_tool_error(e));
                }
            };
            // Untrusted sources skip frontmatter + profile; only grant
            // store entries feed in. The origin scopes the grant-store
            // key so a same-named project skill cannot consume grants
            // approved for a user-level copy. One origin scan feeds
            // both body-trust and entitlement-trust decisions.
            let origin = super::super::skill_body::skill_origin(&*registry, &params.skill)
                .unwrap_or_else(|| "unknown".to_string());
            let untrusted = !super::super::skill_body::is_trusted_origin(&origin);
            if let Some(session) = self.sandbox.as_ref() {
                let (mach, allow_launch) = houyicoder_api::skill::grant::resolve_entitlements(
                    self.skill_grants.as_deref(),
                    &params.skill,
                    &origin,
                    &desc.allowed_mach_services,
                    desc.allow_app_launch,
                    !untrusted,
                );
                session.clear_skill_grants();
                session.set_extra_mach_services(&mach);
                if allow_launch {
                    session.grant_app_launch();
                }
            }
            if let Some(cell) = &self.active_skill {
                *cell.lock().expect("active_skill lock") = Some(params.skill.clone());
            }
            // Register the skill's frontmatter hooks into the session hook
            // registry (invoke-time, session-scoped). The registrar dedups
            // across both invocation paths so a slash dispatch followed by a
            // Skill-tool call for the same skill does not stack a second
            // firing copy. Unwired (None) in tests that do not exercise hooks.
            if let Some(r) = registrar.as_ref() {
                r.register(&*registry, &params.skill);
            }
            // Frame an untrusted body as data so the model treats its
            // directives as unverified. Shared with the slash path.
            let body =
                super::super::skill_body::frame_untrusted_body(&params.skill, &body, untrusted);
            // The grant hook gates session-scoped allowed-tools grants by
            // this trust flag; the model cannot forge it (derived from the
            // registry's origin snapshot, not the body it dresses).
            Ok(json!({
                "skill": params.skill,
                "result": body,
                "allowed_tools": desc.allowed_tools,
                "trusted": !untrusted,
            }))
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn is_read_only(&self) -> bool {
        false
    }
    fn is_destructive(&self) -> bool {
        false
    }
    /// A skill with non-empty allowed_tools requests permission before
    /// executing; safe-only skills auto-allow.
    fn requires_approval_for(&self, input: &Value) -> bool {
        let Some(name) = input.get("skill").and_then(|v| v.as_str()) else {
            return false;
        };
        match self.registry.find(name) {
            Some(desc) => !desc.allowed_tools.is_empty(),
            None => false,
        }
    }
    fn requires_approval(&self) -> bool {
        false
    }
}

/// Map a registry error to the wire tool-error variant the model sees.
/// Reuses the SkillError Display text so the error rendering stays
/// single-sourced. A body-read failure is an I/O error so the cause
/// surfaces distinctly from a wrong name (Failed).
fn skill_error_to_tool_error(e: SkillError) -> ToolError {
    match e {
        SkillError::NotFound(_) => ToolError::Failed(e.to_string()),
        SkillError::BodyLoad(_) => ToolError::Io(e.to_string()),
    }
}

#[cfg(test)]
#[path = "skill_tests.rs"]
mod tests;
