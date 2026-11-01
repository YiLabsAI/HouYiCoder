//! How a delegation's end reads: the outcome its final status names, and the
//! one-line trace the parent timeline keeps of it.

use houyicoder_protocol::frontend::completion::CompletionStatus;

use crate::view::relative_time::summary_span_label;

/// How a child agent ended, as its final fleet status reported it. The mark
/// on the finished row takes its color from this, and the words name the
/// ending rather than repeating a status code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentOutcome {
    /// The child finished the task it was given.
    Finished,
    /// The child stopped on an error.
    Failed,
    /// The child was stopped from outside before it finished.
    Interrupted,
    /// The child stopped at the limit on how many turns it could run.
    TurnLimit,
    /// The child stopped when its token budget ran out.
    Budget,
    /// The child ended on a status word this build does not name. Reading it
    /// as success would claim an ending nobody observed, so it keeps a
    /// neutral word and a neutral color.
    Unknown,
}

impl SubagentOutcome {
    /// The outcome an ending names. An ending this build does not name keeps
    /// the neutral word rather than borrowing a meaning it cannot verify.
    pub(crate) fn from_completion(status: CompletionStatus) -> Self {
        match status {
            CompletionStatus::Completed => Self::Finished,
            CompletionStatus::Failed => Self::Failed,
            CompletionStatus::Killed => Self::Interrupted,
            CompletionStatus::TurnLimit => Self::TurnLimit,
            CompletionStatus::BudgetExhausted => Self::Budget,
            other => {
                tracing::debug!(
                    ?other,
                    "a child reported an ending this build does not name"
                );
                Self::Unknown
            }
        }
    }

    /// The words the finished row shows for this ending.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::TurnLimit => "finished (turn limit)",
            Self::Budget => "finished (budget)",
            Self::Unknown => "ended",
        }
    }
}

/// The terminal moment of one child agent: what the parent timeline records
/// when the child's final status arrives. The row is what lets a reader go
/// back into a delegation after its live view closed, so it carries the
/// child's identity, the task it was given, and how it ended.
///
/// It lives in the frontend's frame log, which means it survives a
/// transcript rebuild for as long as the resident window covers that frame
/// and is not restored from the host's session log. A trace older than the
/// window is gone, exactly like any other row the frontend raised itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentFinished {
    /// The child session the trace names, and the target of a drill-in.
    pub child_sid: String,
    /// The agent type the parent chose for the child.
    pub subagent_type: String,
    /// The task summary the parent handed the child. Empty when the
    /// delegation carried none, and the row then names the agent type.
    pub description: String,
    /// How the child ended.
    pub outcome: SubagentOutcome,
    /// Milliseconds between the first status this frontend saw for the child
    /// and its final one, so the figure measures the observed window rather
    /// than the child's whole life. None when the child was already finished
    /// the first time its status arrived, which is not a zero-length run.
    pub duration_ms: Option<u64>,
    /// Tools the child used, as its final status reported. Zero covers both a
    /// child that used none and a status that carried no figure; the row
    /// leaves the count out rather than present that zero as a measurement.
    pub tool_uses: u32,
}

impl SubagentFinished {
    /// The name the row shows: the task summary, or the agent type when the
    /// delegation carried none.
    ///
    /// Control characters become spaces. The summary is written by the
    /// delegating model, so a newline in it would otherwise split the
    /// one-row trace into two rows that the row count still reported as
    /// one, and the scroll offset would drift from the drawn rows. Invisible
    /// direction marks become spaces for the same reason: they reorder the
    /// drawn glyphs, so the row could read differently from the text it was
    /// given.
    pub fn name(&self) -> String {
        let name = if self.description.is_empty() {
            &self.subagent_type
        } else {
            &self.description
        };
        name.chars().map(display_char).collect()
    }

    /// How the child ended, starting with the quote that closes the name.
    /// It leads the segments after the name because it is the one segment a
    /// row that cannot hold the whole line keeps: a name cut short still says
    /// which delegation ended, while a clipped ending says nothing.
    pub(crate) fn ending(&self) -> String {
        format!("\" {}", self.outcome.label())
    }

    /// The observed run time at the whole-second step a one-line summary
    /// reads at, or None when the child was never seen running.
    pub(crate) fn duration_text(&self) -> Option<String> {
        self.duration_ms
            .map(|ms| format!(" \u{00b7} {}", summary_span_label(ms)))
    }

    /// The tool count, singular for one, or nothing when the status carried
    /// no figure. The row leaves the segment out rather than report a count
    /// no status measured.
    pub(crate) fn tools_text(&self) -> String {
        if self.tool_uses == 0 {
            return String::new();
        }
        let unit = if self.tool_uses == 1 { "tool" } else { "tools" };
        format!(" \u{00b7} {} {unit}", self.tool_uses)
    }

    /// The whole trace as one row of text: the mark, the agent, the quoted
    /// name, the ending, and the measured segments.
    pub fn text(&self) -> String {
        let mut text = format!("\u{25cf} Agent \"{}{}", self.name(), self.ending());
        if let Some(duration) = self.duration_text() {
            text.push_str(&duration);
        }
        text.push_str(&self.tools_text());
        text
    }
}

/// A character as the one-row trace may draw it: control characters and the
/// invisible direction marks that reorder a line become spaces. The summary
/// is written by the delegating model, so it is untrusted for display.
fn display_char(c: char) -> char {
    let reorders = matches!(
        c,
        '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    );
    if c.is_control() || reorders { ' ' } else { c }
}

#[cfg(test)]
mod tests;
