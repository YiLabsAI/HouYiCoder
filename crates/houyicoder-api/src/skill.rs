//! The skill registry port: the engine-facing contract for discovering
//! skills and preparing their bodies. The concrete implementation (which
//! reads SKILL.md files from disk) lives in the composition root; the
//! engine depends on this trait so it does not depend on the skill data
//! crate directly. Object-safe (sync methods) so the engine holds
//! Arc<dyn SkillRegistry> and the concrete registry swaps behind it.
//!
//! Methods are synchronous: the discovered set is cached at startup, and
//! body preparation is a one-shot file read plus string substitution.
//! The Skill tool wraps them inside its async execute.

use std::fmt;

/// A minimal, engine-facing view of a discovered skill. Carries only the
/// fields the engine consumes (listing, invocation gating, cost visibility);
/// the full parsed definition stays in the skill data crate and never
/// crosses this port.
#[derive(Debug, Clone)]
pub struct SkillDescriptor {
    /// Identity (directory name). Matches ^[a-z0-9-]+$.
    pub name: String,
    /// One-line description for the model-visible listing.
    pub description: String,
    /// Optional "when to use" guidance appended to the listing entry.
    pub when_to_use: Option<String>,
    /// Optional argument hint shown in the slash palette.
    pub argument_hint: Option<String>,
    /// True when the skill is hidden from the model-visible listing and
    /// blocked from Skill-tool invocation (frontmatter
    /// disable-model-invocation). User slash dispatch is unaffected.
    pub disable_model_invocation: bool,
    /// True when the user cannot invoke the skill via slash (frontmatter
    /// user-invocable: false). Model invocation is unaffected.
    pub user_invocable: bool,
    /// Rough body token estimate (bytes / 4) so the model and the host
    /// see the invocation cost before committing.
    pub body_token_estimate: u32,
    /// Additive tool grants from frontmatter allowed-tools. Non-empty means
    /// the skill requests permission-bearing properties, so the Skill tool
    /// asks before executing (the safe-property allowlist gate).
    pub allowed_tools: Vec<String>,
}

/// Errors a skill registry can return when preparing a body.
#[derive(Debug, Clone)]
pub enum SkillError {
    /// No skill with the given name was found in the discovered set.
    NotFound(String),
    /// The body file could not be read (missing, permission, io).
    BodyLoad(String),
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillError::NotFound(name) => write!(f, "skill not found: {name}"),
            SkillError::BodyLoad(msg) => write!(f, "skill body load failed: {msg}"),
        }
    }
}

impl std::error::Error for SkillError {}

/// The engine-facing skill registry. The Skill tool + the slash dispatch
/// both call find (to gate on their own invocation flag) then prepare_body
/// (the shared body-prep, ungated — the two paths converge there). The
/// turn-entry listing step calls list_model_invocable. The concrete
/// implementation wraps the skill data crate (discovery + body
/// preparation) and is constructed at the composition root.
pub trait SkillRegistry: Send + Sync {
    /// Skills visible to the model (disable-model-invocation filtered out),
    /// in precedence order. Used to build the per-turn listing attachment.
    fn list_model_invocable(&self) -> Vec<SkillDescriptor>;

    /// Look up a skill by name. The caller checks the invocation flag
    /// (disable-model-invocation for the Skill tool, user-invocable for
    /// slash) before preparing the body — gating is the caller's job,
    /// the registry only resolves + describes. None when no skill
    /// matches.
    fn find(&self, name: &str) -> Option<SkillDescriptor>;

    /// Load + prepare the body for a named skill: read the body file,
    /// strip frontmatter, prepend the base-dir header, substitute
    /// arguments and variables. Ungated — the caller gates on the
    /// invocation flag via find. Returns NotFound when no skill matches,
    /// BodyLoad when the body file could not be read. The session id
    /// feeds variable substitution; None when the dispatch is not
    /// session-bound.
    fn prepare_body(
        &self,
        name: &str,
        args: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<String, SkillError>;

    /// All discovered skills paired with their discovery origin (managed /
    /// user / project / claude_eco / agents / mcp / local), for surfaces that
    /// group skills by source — the /skills pane. Unlike
    /// list_model_invocable, this does NOT filter disable-model-invocation
    /// skills: the visibility surface shows them (marked not invocable) so
    /// the user can see they are blocked from the model. The origin is
    /// carried alongside the descriptor (rather than on the descriptor) so
    /// SkillDescriptor stays under the field-count warn line. The default
    /// returns empty: a registry that does not track origin reports no
    /// grouped skills, so production registries override this.
    fn list_with_origin(&self) -> Vec<SkillSnapshot> {
        Vec::new()
    }

    /// Detect skill-directory script executions in a Bash command. Returns one
    /// entry per script the command runs from a discovered skill's directory,
    /// carrying the skill + relative script path for the approval card. The
    /// default returns empty: a registry that does not track skill directories
    /// reports no scripts, so the existing protected-path ask still surfaces
    /// but without the script path.
    fn detect_run_scripts(&self, _command: &str) -> Vec<SkillScriptRef> {
        Vec::new()
    }
}

/// A model-invocable descriptor paired with where it was discovered, for
/// source-grouped surfaces. See list_with_origin.
#[derive(Debug, Clone)]
pub struct SkillSnapshot {
    pub descriptor: SkillDescriptor,
    /// snake_case discovery source (managed/user/project/claude_eco/agents/
    /// mcp/local). Empty when the registry does not track origin.
    pub origin: String,
}

/// A skill-directory script a Bash command runs, surfaced for the per-script
/// confirmation card. The approval prompt shows the skill + relative script
/// path so the user can see what would execute before approving. The path is
/// verifiable (the user can read the file); a first-line summary is NOT shown,
/// because it is attacker-controlled text the card would frame as an
/// authoritative summary. The detection is a heuristic over the command string
/// and the discovered skill directories; a deliberately obfuscated command
/// can evade it, but the sandbox fence remains the hard floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillScriptRef {
    /// The skill whose directory the script lives under.
    pub skill_name: String,
    /// Path relative to the skill directory, e.g. "scripts/deploy.py".
    pub script_rel_path: String,
}
