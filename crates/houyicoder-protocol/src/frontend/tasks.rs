//! The words and snapshots a background command reports to a frontend.
//!
//! A command that outlives its tool call keeps running on the host, so the
//! frontend learns about it from periodic snapshots rather than from the tool
//! result. The host assigns the number and owns the state; a frontend holds
//! only the latest snapshot and replaces it by number.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The number a host assigns to one command that outlived its tool call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BackgroundTaskId(pub u64);

impl BackgroundTaskId {
    /// The form a row shows for this task.
    pub fn label(&self) -> String {
        format!("#{}", self.0)
    }
}

/// Where one background command stands.
///
/// The text form is the protocol's stable vocabulary. An unknown word decodes
/// to Unknown rather than failing the frame, so a newer host reporting a state
/// this build does not name still delivers the rest of its snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BackgroundTaskStatus {
    /// The command is still running.
    Running,
    /// The command exited with a zero status.
    Finished,
    /// The command exited with a non-zero status, ended without a status, or
    /// could not be started.
    Failed,
    /// The user stopped the command before it ended.
    Killed,
    /// The command reached its own wall-clock budget.
    TimedOut,
    /// A state this build does not name, kept as received.
    Unknown(String),
}

impl BackgroundTaskStatus {
    /// The text form for this state.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::TimedOut => "timed_out",
            Self::Unknown(word) => word,
        }
    }

    /// The state a text form names. A word outside the known set becomes
    /// Unknown and keeps the word.
    pub fn from_word(word: &str) -> Self {
        match word {
            "running" => Self::Running,
            "finished" => Self::Finished,
            "failed" => Self::Failed,
            "killed" => Self::Killed,
            "timed_out" => Self::TimedOut,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// Whether the task has ended. A state this build does not name is not
    /// taken as an ending: only the endings it can name are, so an unknown
    /// word keeps the pane reading rather than freezing it on a guess.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished | Self::Failed | Self::Killed | Self::TimedOut
        )
    }
}

impl Serialize for BackgroundTaskStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for BackgroundTaskStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        Ok(Self::from_word(&word))
    }
}

/// One background command as a frontend holds it: the latest snapshot for a
/// task number, replacing whatever the previous frame carried. The list and
/// the changed event carry identity and state, not output: a live preview
/// belongs to the detail, read on demand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTaskSnapshot {
    /// The number the host assigned.
    pub id: BackgroundTaskId,
    /// Where the command stands.
    pub status: BackgroundTaskStatus,
    /// The first line of the command, as a row shows it.
    pub command: String,
    /// Whole milliseconds since the command started.
    pub elapsed_ms: u64,
    /// The exit status once the command ended. Absent while running, and for a
    /// command a signal ended.
    pub exit_code: Option<i32>,
}

/// One background command's full record, read on demand by the detail view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundTaskDetail {
    /// The number the host assigned.
    pub id: BackgroundTaskId,
    /// Where the command stands at the moment of the read.
    pub status: BackgroundTaskStatus,
    /// The first line of the command.
    pub command: String,
    /// Whole milliseconds since the command started, frozen once it ended.
    pub elapsed_ms: u64,
    /// The exit status once the command ended.
    pub exit_code: Option<i32>,
    /// The command's standard output, bounded the way a tool result is.
    pub stdout: String,
    /// The command's standard error, bounded the way a tool result is.
    pub stderr: String,
    /// The most recent output lines at the moment of the read.
    #[serde(default)]
    pub tail: Vec<String>,
}

#[cfg(test)]
mod tests;
