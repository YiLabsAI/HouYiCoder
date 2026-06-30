//! Tool-list wire mirror. The /tools command asks the server for the
//! registered tools; this is the typed payload the response carries so the
//! TUI renders the list without importing the engine registry.

use serde::{Deserialize, Serialize};

/// One registered tool: its invocation name plus a short description (the
/// first line of the tool's help text is the brief the render shows).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolEntry {
    pub name: String,
    pub description: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trips_camel_case() {
        let entry = ToolEntry {
            name: "bash".into(),
            description: "run a shell command".into(),
        };
        let json = serde_json::to_string(&entry).expect("serialize");
        assert!(json.contains("\"name\":\"bash\""), "{json}");
        assert!(json.contains("\"description\":\"run"), "{json}");
        let back: ToolEntry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, entry);
    }
}
