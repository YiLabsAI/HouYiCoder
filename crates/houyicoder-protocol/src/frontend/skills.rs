//! Skill-list wire mirror. The /skills command asks the server for the
//! discovered skills; this is the typed payload the response carries so the
//! TUI renders the list without importing the skill data crate.

use serde::{Deserialize, Serialize};

/// One discovered skill: name (directory name), description, origin (where it
/// was discovered, for grouping), invocable (whether the model may call it),
/// a rough body token estimate so the user sees the invocation cost, and
/// session-scoped usage stats so the /skills detail shows how often the
/// skill was invoked or refused this session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// Discovery source snake_case (managed/user/project/claude_eco/agents/
    /// mcp) — the /skills pane groups entries by this field.
    pub origin: String,
    /// False when frontmatter disable-model-invocation hides the skill from
    /// the model; the pane flags it so the user knows it is not callable.
    pub invocable: bool,
    /// True when the user can invoke the skill via @skill:name (frontmatter
    /// user-invocable). A skill can be !invocable (model can't auto-call)
    /// but user_invocable=true (user can still @skill: it).
    #[serde(default)]
    pub user_invocable: bool,
    pub body_token_estimate: u32,
    /// Session-scoped invocation stats. None when the registry does not track
    /// usage (old payloads deserialize to None via serde default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SkillUsage>,
}

/// Wire mirror of the api SkillUsage. invocations counts successful body
/// preparations; refusals counts gate rejections; last_used_secs is the
/// epoch-seconds of the most recent invocation (0 when never invoked).
/// These measure call acceptance, NOT skill quality — the true outcome
/// lives downstream in the turn that consumed the body.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillUsage {
    pub invocations: u64,
    pub refusals: u64,
    pub last_used_secs: u64,
}

#[cfg(test)]
mod tests;
