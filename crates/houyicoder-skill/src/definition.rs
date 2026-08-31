//! Skill definition: the parsed SKILL.md frontmatter + body path.
//! This is the prompt command definition type — skills are one view of
//! it, custom slash commands will be another. loaded_from distinguishes
//! the source.

use std::path::PathBuf;

/// Where a skill was discovered. Determines precedence (managed >
/// project > user) and trust gating.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    Managed,
    User,
    Project,
    /// .claude/skills — ecosystem compat path (zero-migration reuse).
    ClaudeEco,
    /// .agents/skills/ — AgentSkill spec interop convention.
    Agents,
    /// MCP server prompts surfaced as skills.
    Mcp,
    /// Gitignored local overrides (not committed to the repo).
    Local,
}

/// Execution context for a skill invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillContext {
    /// Body injected into the current agent message stream.
    Inline,
    /// Skill runs in a spawned child agent. The agent type is resolved
    /// at invocation; if not found the call is rejected (no random
    /// fallback — deny-by-default).
    Fork(String),
    /// Skill name preloaded into a child agent listing at spawn time
    /// (allowlist semantics: the child only sees these skills).
    Preload,
}

/// A parsed SKILL.md skill. The identity is the directory name; the
/// frontmatter name field is a display-name override (warned on
/// mismatch, not rejected). Fields with v0-ignored semantics are
/// parsed and stored but have no runtime behavior yet.
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    /// Directory name = identity (spec: must match ^[a-z0-9-]+$).
    pub name: String,
    /// Frontmatter name override (display only).
    pub display_name: Option<String>,
    /// Required; missing skips the file (except skills/bundled sources
    /// which derive from the first markdown line).
    pub description: String,
    pub when_to_use: Option<String>,
    /// Additive grant: adds always-allow rules for the skill duration.
    /// Session-scoped, not persistent (inline has no end event).
    pub allowed_tools: Vec<String>,
    pub argument_hint: Option<String>,
    pub version: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// If true, the skill is excluded from the model-visible listing.
    pub disable_model_invocation: bool,
    /// If false (default true), the skill cannot be invoked via slash.
    pub user_invocable: bool,
    /// Execution context hint from frontmatter context: fork.
    pub context: SkillContext,
    /// Conditional activation: gitignore-style path patterns. The skill
    /// activates for the session when a touched file matches.
    pub paths: Vec<String>,
    pub source: SkillSource,
    /// Absolute path to the SKILL.md file (body read on demand at
    /// invocation time, not at load).
    pub body_path: PathBuf,
    /// Absolute path to the skill directory (for base-dir header +
    /// variable substitution + resource access).
    pub skill_dir: PathBuf,
    /// Raw YAML value of the hooks frontmatter field. Stored but not
    /// deep-parsed — 0% of community skills use hooks in frontmatter;
    /// deep parsing is deferred until a real skill needs it.
    pub hooks_raw: Option<serde_yaml::Value>,
    /// AgentSkill spec fields (license, compatibility, metadata).
    /// Passed through, not consumed in v0.
    pub spec_fields: SpecFields,
    /// Other unknown frontmatter fields, stored as-is for forward
    /// compatibility (pass-through, not rejected).
    pub unknown_fields: serde_yaml::Mapping,
}

/// The six AgentSkill spec fields. Parsed and stored; v0 surfaces
/// license in /skills but does not act on compatibility/metadata.
#[derive(Debug, Clone, Default)]
pub struct SpecFields {
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub metadata: serde_yaml::Mapping,
}

impl SkillDefinition {
    /// Estimated body token count for listing display. Lets the model
    /// make informed invocation decisions (cost-visible). Reads the body
    /// file; the registry caches the result at construction so listing +
    /// find do not re-read per call. Rough estimate (bytes / 4); a real
    /// tokenizer would replace the divisor without changing the contract.
    pub fn body_token_estimate(&self) -> u32 {
        match std::fs::read_to_string(&self.body_path) {
            Ok(content) => (content.len() / 4) as u32,
            Err(_) => 0,
        }
    }
}
