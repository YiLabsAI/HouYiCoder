//! Why a notification was injected into a session's context. The durable
//! record and every reader share one vocabulary, so a reader never re-derives
//! the reason from the text a notification carries.

/// Why a notification was injected into a session's context.
///
/// The durable record carries the word; code branches on this enum, so a new
/// reason is declared here rather than spelled again at every reader. A word
/// written by a build that knows a reason this one does not reads as None, and
/// the reader keeps the framing it already applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationTopic {
    /// A background subagent reported the result of a delegated task.
    ChildCompletion,
    /// A command that outlived its tool call reached a terminal state.
    Task,
}

impl NotificationTopic {
    /// The word the durable record carries.
    pub fn as_word(self) -> &'static str {
        match self {
            Self::ChildCompletion => "completion",
            Self::Task => "task",
        }
    }

    /// The reason a recorded word names, or None for a word this build does
    /// not know.
    pub fn from_word(word: &str) -> Option<Self> {
        match word {
            "completion" => Some(Self::ChildCompletion),
            "task" => Some(Self::Task),
            _ => None,
        }
    }
}
