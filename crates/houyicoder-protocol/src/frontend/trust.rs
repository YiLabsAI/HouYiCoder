//! Workspace trust prompt wire payloads. The host sends a TrustPrompt as a
//! server-to-client reverse request at startup when the project workspace is
//! not yet trusted (one-time ack, not a per-action ask). The client replies
//! with TrustAccept; a reject shuts the session down. Distinct from
//! ServerRequestPayload::Permission, which is a per-tool verdict mid-turn.

use serde::{Deserialize, Serialize};

/// A workspace-trust reverse request. Fires once at startup before the run
/// loop, never per-call: trust is a property of the folder, not of an
/// individual skill. The answer persists in user-level settings keyed by
/// project path, so the prompt does not repeat for a path already trusted.
/// Ancestor trust covers descendants: trusting a parent trusts its children.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustPrompt {
    /// The canonical project path being asked about, rendered so the user
    /// sees exactly which folder requests trust.
    pub project_path: String,
    /// Project-level config that would execute code if trusted, so the user
    /// decides with risks visible (skills whose allowed-tools include Bash,
    /// project hooks, project MCP servers). Empty when none is found; trust
    /// is still asked because project skills and hooks load regardless.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub risks: Vec<TrustRisk>,
}

/// One risk line in a TrustPrompt. The kind field is skill_bash, hook, or
/// mcp_server; the client groups by kind for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustRisk {
    pub kind: String,
    pub name: String,
}

/// The client answer to a TrustPrompt. accepted false declines (the host
/// shuts down with no partial trust); accepted true persists the path as
/// trusted so the prompt does not repeat for it or its descendants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrustAccept {
    pub accepted: bool,
}

#[cfg(test)]
mod tests;
