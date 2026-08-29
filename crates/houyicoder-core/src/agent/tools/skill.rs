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

use std::sync::Arc;

use houyicoder_api::skill::{SkillError, SkillRegistry};
use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_async::PFut;
use houyicoder_protocol::extension::ToolError;
use serde::Deserialize;
use serde_json::{Value, json};

/// The Skill tool. Holds a SkillRegistry port; the concrete registry
/// (which reads SKILL.md files from disk) is constructed at the
/// composition root and injected here. Read-only: the tool loads and
/// returns text; it mutates no external state.
pub struct SkillTool {
    registry: Arc<dyn SkillRegistry>,
}

impl SkillTool {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self { registry }
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
                return Err(ToolError::Failed(format!(
                    "skill {} is disabled for model invocation",
                    params.skill
                )));
            }
            let body = registry
                .prepare_body(&params.skill, params.args.as_deref(), sid.as_deref())
                .map_err(skill_error_to_tool_error)?;
            // Frame an untrusted body (a non-managed/user source) as data
            // so the model treats its directives as unverified and confirms
            // before state-changing steps. A tool result is already data,
            // but a tool-result role alone does not tell the model to
            // confirm before acting; the framing note does. Shared with
            // the slash path so framing does not differ by invocation path.
            let untrusted = super::super::skill_body::origin_untrusted(&*registry, &params.skill);
            let body =
                super::super::skill_body::frame_untrusted_body(&params.skill, &body, untrusted);
            // Carry the trust decision so the grant hook gates the
            // session-scoped allowed-tools grant by source: only a managed
            // or user source may install an always-allow for its tools, so a
            // project or ecosystem skill's tools re-ask on each call. This
            // field is engine-internal metadata the grant hook reads; the
            // model cannot forge it (derived from the registry's origin
            // snapshot, independent of the body it dresses).
            Ok(json!({
                "skill": params.skill,
                "result": body,
                "allowed_tools": desc.allowed_tools,
                "trusted": !untrusted,
            }))
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }
    fn is_destructive(&self) -> bool {
        false
    }
    /// Safe-property allowlist gate: a skill with non-empty allowed_tools
    /// requests permission-bearing properties (tools the skill grants to
    /// the session), so the loop asks before executing. A skill with only
    /// safe properties (name, description, when-to-use, etc.) is
    /// auto-allowed — no Ask. Input-aware: parses the skill name from the
    /// call input, resolves it via the registry, and checks its
    /// allowed_tools. An unknown skill or missing name does not Ask (the
    /// execute will fail with NotFound, a clearer signal than an approval
    /// gate).
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
mod tests {
    use super::*;
    use houyicoder_api::skill::{SkillDescriptor, SkillError, SkillRegistry, SkillSnapshot};
    use std::collections::HashMap;

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
}
