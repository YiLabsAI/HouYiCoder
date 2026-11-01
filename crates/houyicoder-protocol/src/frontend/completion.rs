//! How a child agent ended, stated once for the readers that report it.
//!
//! A child's finish notification, the fleet relay's final snapshot, and the
//! frontend's own ending words were three tables of the same five strings,
//! kept in step manually. This is the one table for those three.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The terminal state a reader is told a child agent reached.
///
/// It reports the ending, not the engine's own run outcome: the engine keeps
/// RunCompletionStatus and the host maps one onto the other at its boundary.
/// The text form is the protocol's vocabulary and is stable: it is what an
/// older frontend reads. An unknown word decodes to Unknown rather than
/// failing the frame, so a newer host reporting an ending this build does not
/// name still delivers the rest of its status.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompletionStatus {
    /// The work produced its final output.
    Completed,
    /// The work stopped on an error.
    Failed,
    /// The work was stopped from outside before it finished.
    Killed,
    /// The work stopped at its limit on how many turns it could run.
    TurnLimit,
    /// The work stopped when its token budget ran out. The engine no longer
    /// reaches this ending on its own, but a released host reported it, and
    /// a word a peer sent must still decode to the ending it named rather
    /// than to Unknown.
    BudgetExhausted,
    /// An ending this build does not name. Only from_word produces it, and
    /// only for a word outside the known set. The word is kept as received,
    /// so a re-encode sends the reporting side's own word unchanged.
    Unknown(String),
}

impl CompletionStatus {
    /// The text form for this ending.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::TurnLimit => "turn_limit",
            Self::BudgetExhausted => "budget",
            Self::Unknown(word) => word,
        }
    }

    /// The ending a text form names. A word outside the known set becomes
    /// Unknown and keeps the word.
    pub fn from_word(word: &str) -> Self {
        match word {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "killed" => Self::Killed,
            "turn_limit" => Self::TurnLimit,
            "budget" => Self::BudgetExhausted,
            other => Self::Unknown(other.to_string()),
        }
    }
}

impl Serialize for CompletionStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CompletionStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        Ok(Self::from_word(&word))
    }
}

#[cfg(test)]
mod tests;
