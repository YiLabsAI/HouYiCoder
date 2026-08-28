//! Skill-list wire mirror. The /skills command asks the server for the
//! discovered skills; this is the typed payload the response carries so the
//! TUI renders the list without importing the skill data crate.

use serde::{Deserialize, Serialize};

/// One discovered skill: name (directory name), description, origin (where it
/// was discovered, for grouping), invocable (whether the model may call it),
/// and a rough body token estimate so the user sees the invocation cost.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// Discovery source snake_case (managed/user/project/claude_eco/agents/
    /// mcp/local) — the /skills pane groups entries by this field.
    pub origin: String,
    /// False when frontmatter disable-model-invocation hides the skill from
    /// the model; the pane flags it so the user knows it is not callable.
    pub invocable: bool,
    pub body_token_estimate: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trips_camel_case() {
        let entry = SkillEntry {
            name: "commit".into(),
            description: "commit changes".into(),
            origin: "user".into(),
            invocable: true,
            body_token_estimate: 120,
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(json.contains("\"name\":\"commit\""), "{json}");
        assert!(json.contains("\"origin\":\"user\""), "{json}");
        assert!(json.contains("\"invocable\":true"), "{json}");
        assert!(json.contains("\"bodyTokenEstimate\":120"), "{json}");
        let back: SkillEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, entry);
    }
}
