//! Wire types for the /debug command. The level is a small enum rather than
//! a string so a typo cannot ask the server for a level it does not know;
//! the state carries the file path so the host can tell the user where to
//! look without guessing.

use serde::{Deserialize, Serialize};

/// The diagnostic level the /debug command can set. Maps to the tracing
/// subscriber's filter; the service layer translates to LevelFilter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DebugLevel {
    /// Stop recording. Events are discarded at the macro call site.
    Off,
    /// Record at debug level and above: lifecycle points, best-effort
    /// failures, retries. The level a session is normally diagnosed at.
    Debug,
}

/// The diagnostic state reported back to the host. Carried in the response
/// to a DebugSet request so the system line can show the path the user
/// should look at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugState {
    /// Whether the sink is currently recording.
    pub enabled: bool,
    /// The file path the sink writes to, fixed at install time.
    pub path: String,
}
