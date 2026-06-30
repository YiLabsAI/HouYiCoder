//! Wire-level hook metadata for the /hooks visibility command. The server
//! converts the runner's HookRegistry::list() into these serializable entries
//! and ships them to the client for read-only display.

use serde::{Deserialize, Serialize};

/// One hook as seen by the client (the /hooks command). Carries the hook's
/// name, the events it subscribes to, and its config source so the user can
/// see what is wired without digging through config files. For a framework
/// event (not a registered external hook), source is "framework" and fired
/// reports whether the event has a live fire point in the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookEntry {
    pub name: String,
    pub events: Vec<String>,
    pub source: String,
    #[serde(default)]
    pub fired: bool,
    #[serde(default)]
    pub summary: String,
}
