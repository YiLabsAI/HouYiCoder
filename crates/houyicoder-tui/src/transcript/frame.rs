//! One entry of the transcript's frame log: a server frame in arrival order,
//! or a row the frontend raises on its own account. Both fold through the
//! same pipeline, so a frontend row keeps its place among the server's rows
//! instead of drifting to wherever a rebuild happens to put it.

use houyicoder_protocol::acpx::AcpxNotification;
use houyicoder_protocol::envelope::ChildTranscriptFrame;
use houyicoder_protocol::frontend::session_update::SessionUpdate;

use crate::transcript::records::{ContextView, SubagentFinished, TranscriptLine};

pub(crate) mod payload;

/// One frame of the turn stream, preserved in arrival order so the
/// transcript rebuild keeps the time-ordered interleave of session/update
/// chunks and acpx/context/* audit notifications (a compaction checkpoint
/// lands between the tool calls that bracketed it, not at the tail). The
/// driver accumulates the server's frames as it pushes them, and the
/// frontend appends its own rows to the same log; the transcript is a
/// faithful fold of that ordered stream, never a stub.
#[derive(Debug, Clone)]
pub enum TranscriptFrame {
    /// An ACP session/update chunk (user / agent / thought message, tool call,
    /// tool-call update). The standard turn stream the base protocol carries.
    Session(SessionUpdate),
    /// An acpx/* extension notification — durable-context audit records the
    /// base session/update has no variant for (compaction boundary, summary,
    /// permission decision, meta user), or a token-level provider event.
    Acpx(AcpxNotification),
    /// A row the frontend raises on its own account, listed in FrontendRow:
    /// no server frame carries it, so the frontend puts it in this log at the
    /// position it happened. Every view of the log renders it there — the live
    /// turn, a window that slid past older frames, scrollback loading frames
    /// above it — and a view that no longer covers its frame shows it no more.
    Frontend(FrontendRow),
}

/// A row the frontend raises on its own account. It renders through the same
/// fold as the server's frames, so it lands where it was raised and
/// stays there while the window slides, rather than drifting to the head of
/// whatever rows a rebuild happens to keep.
#[derive(Debug, Clone)]
pub enum FrontendRow {
    /// A system line: command feedback, a notice, a warning, a failure.
    System(String),
    /// A /context breakdown rendered inline as conversation content. Boxed:
    /// the view is far larger than any other row, and every row is copied on
    /// a rebuild.
    Context(Box<ContextView>),
    /// The abort notice, welded under the message the abort cut off.
    Interrupted,
    /// The notice that the interrupted submission went back to the input box.
    InputRestored,
    /// The echo of a submitted command.
    Echo(String),
    /// A child agent's terminal moment, raised when its final status arrives
    /// and kept in the log so the delegation still has a trace after its view
    /// closes.
    SubagentFinished(SubagentFinished),
}

impl FrontendRow {
    /// The row the frontend raises for a line no server frame carries, or None
    /// for a line the log itself reproduces. A user message the frontend echoes
    /// back as a prompt row is tentative: it renders from its own Echo frame
    /// until the server sends the UserMessageChunk for the same text, at which
    /// point push_frame drops the Echo so the row renders once from the server
    /// frame. The text decides nothing about routing: a slash echo is still
    /// frontend-raised, and a plain message echo is too.
    pub(crate) fn from_line(line: &TranscriptLine) -> Option<Self> {
        match line {
            TranscriptLine::System(text) => Some(Self::System(text.clone())),
            TranscriptLine::ContextGrid(view) => Some(Self::Context(Box::new(view.clone()))),
            TranscriptLine::Interrupted => Some(Self::Interrupted),
            TranscriptLine::User(text) => Some(Self::Echo(text.clone())),
            TranscriptLine::SubagentFinished(child) => Some(Self::SubagentFinished(child.clone())),
            _ => None,
        }
    }

    /// The row this frame renders as.
    pub(crate) fn line(&self) -> TranscriptLine {
        match self {
            Self::System(text) => TranscriptLine::System(text.clone()),
            Self::Context(view) => TranscriptLine::ContextGrid((**view).clone()),
            Self::Interrupted => TranscriptLine::Interrupted,
            Self::InputRestored => TranscriptLine::System("input restored".into()),
            Self::Echo(text) => TranscriptLine::User(text.clone()),
            Self::SubagentFinished(child) => TranscriptLine::SubagentFinished(child.clone()),
        }
    }
}

/// Convert a fetched child frame to the live-frame shape the fold
/// reads, so child rows render through the same pipeline as the parent flow.
impl From<ChildTranscriptFrame> for TranscriptFrame {
    fn from(frame: ChildTranscriptFrame) -> Self {
        match frame {
            ChildTranscriptFrame::Session(u) => TranscriptFrame::Session(u),
            ChildTranscriptFrame::Acpx(n) => TranscriptFrame::Acpx(n),
        }
    }
}
