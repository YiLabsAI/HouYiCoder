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
}

/// Errors a skill registry can return when preparing a body.
#[derive(Debug, Clone)]
pub enum SkillError {
    /// No skill with the given name was found in the discovered set.
    NotFound(String),
    /// The skill exists but frontmatter disable-model-invocation blocks
    /// Skill-tool invocation. Returned only from the Skill-tool path.
    NotModelInvocable(String),
    /// The body file could not be read (missing, permission, io).
    BodyLoad(String),
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkillError::NotFound(name) => write!(f, "skill not found: {name}"),
            SkillError::NotModelInvocable(name) => {
                write!(f, "skill {name} is disabled for model invocation")
            }
            SkillError::BodyLoad(msg) => write!(f, "skill body load failed: {msg}"),
        }
    }
}

impl std::error::Error for SkillError {}

/// The engine-facing skill registry. The Skill tool holds an
/// Arc<dyn SkillRegistry> and calls prepare_body at invocation time; the
/// turn-entry listing step calls list_model_invocable to build the
/// listing attachment. The concrete implementation wraps the skill data
/// crate (discovery + body preparation) and is constructed at the
/// composition root.
pub trait SkillRegistry: Send + Sync {
    /// Skills visible to the model (disable-model-invocation filtered out),
    /// in precedence order. Used to build the per-turn listing attachment.
    fn list_model_invocable(&self) -> Vec<SkillDescriptor>;

    /// Load + prepare the body for a named skill: read the body file,
    /// strip frontmatter, prepend the base-dir header, substitute
    /// arguments and variables. Returns NotModelInvocable when the
    /// skill is gated for model invocation, NotFound when no skill
    /// matches. The session id feeds variable substitution; None when
    /// the dispatch is not session-bound.
    fn prepare_body(
        &self,
        name: &str,
        args: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<String, SkillError>;
}
