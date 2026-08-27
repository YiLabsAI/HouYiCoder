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
}
