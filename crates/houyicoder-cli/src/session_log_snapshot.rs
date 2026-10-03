//! The session-log implementation of the TUI's TranscriptSnapshot port: the
//! search view reads the durable session log through the shared history
//! reader, maps each event to the frames the live stream carries, and folds
//! them through the same projection the live render uses, so scrolling
//! through history shows the turns the live view showed. Each frame carries
//! the durable identity of the event that produced it, so a turn's summary
//! row keeps the same name in every windowed read and never collides with
//! the frame-index names the live transcript carries.
//!
//! The mappings are the service layer's map_session_update and
//! map_acpx_notification, the same two the live push uses. A local copy would
//! drift, as one already did when it left out the interruption notice.
//!
//! Run state is what the two paths cannot share: this one reads a log written
//! by another process, so a turn that no record closes stays open here.
//!
//! The byte windows, the lazy offset index, and the turn lookback belong to
//! SessionHistory; this type only projects what that reader returns. G
//! triggers a full index build with progress.

use std::sync::Arc;

#[cfg(test)]
use houyicoder_api::session::SessionLog;
#[cfg(test)]
use houyicoder_context::SessionId;
use houyicoder_context::{SessionEvent, SessionLogEntry};
use houyicoder_service::protocol_adapter::{map_acpx_notification, map_session_update};
use houyicoder_tui::records::TranscriptLine;
use houyicoder_tui::transcript::snapshot::{
    IndexProgress, SnapshotLoad, TranscriptSnapshot, WindowLoad,
};
use houyicoder_tui::transcript::{TranscriptFrame, bounds_turn_in, transcript_from_named_frames};

use crate::session_history::{EventWindow, LocatedEvent, SessionHistory};

/// The window read budget: 256 KB per screen (~70 events at 3.6 KB avg).
#[cfg(test)]
const WINDOW_MAX_BYTES: u64 = 256 * 1024;

/// The session-log implementation of the TranscriptSnapshot port: holds the
/// shared history reader and projects its windows to rendered lines.
pub struct SessionLogSnapshot {
    history: Arc<SessionHistory>,
}

impl SessionLogSnapshot {
    /// Convenience for tests, which build a snapshot without a shared reader.
    #[cfg(test)]
    pub fn new(session_log: Arc<dyn SessionLog>, session_id: SessionId) -> Self {
        Self {
            history: Arc::new(SessionHistory::new(session_log, session_id)),
        }
    }

    /// Build the snapshot over a history reader the caller already owns, so
    /// the transcript and the trajectory share one set of byte windows and one
    /// offset index instead of each walking the log on its own.
    pub fn with_history(history: Arc<SessionHistory>) -> Self {
        Self { history }
    }

    /// Map a durable event to the frames the projection reads, through the
    /// same two mappers the live push uses: the session/update stream and the
    /// acpx notifications it has no variant for. Both are needed — a run
    /// completion record arrives as a notification, and it is what tells the
    /// projection where a turn ended.
    fn frames_of(event: &SessionEvent) -> [Option<TranscriptFrame>; 2] {
        [
            map_session_update(event).map(TranscriptFrame::Session),
            map_acpx_notification(event).map(TranscriptFrame::Acpx),
        ]
    }

    /// The frames a run of durable events renders to, paired with the row
    /// name each frame carries: the identity of the event that produced it.
    /// Both frames of one event share its identity, so a turn's summary row
    /// is named by the event the log recorded the close in — the same name
    /// in every read, whatever window it started from. The identity type
    /// belongs to the log's crate, so the formatting lives here rather than
    /// in the projection.
    fn named_frames<'a>(
        events: impl IntoIterator<Item = &'a SessionLogEntry>,
    ) -> (Vec<TranscriptFrame>, Vec<String>) {
        let mut frames = Vec::new();
        let mut names = Vec::new();
        for event in events {
            for frame in Self::frames_of(&event.event).into_iter().flatten() {
                names.push(format!("e{}", event.id));
                frames.push(frame);
            }
        }
        (frames, names)
    }

    /// Render a run of durable events to transcript lines. The snapshot has
    /// no run state to consult, so a turn its log carries no record for is
    /// left open: the snapshot never claims a turn ended that the log does
    /// not record as ended.
    fn project_events<'a>(
        events: impl IntoIterator<Item = &'a SessionLogEntry>,
    ) -> Vec<TranscriptLine> {
        let (frames, names) = Self::named_frames(events);
        transcript_from_named_frames(&frames, &names, 0..frames.len(), true)
    }

    /// Whether a step of events holds where a turn begins or ends, in the
    /// sense the projection reads: a message that opened a turn, or the record
    /// that closed one. The probe reads frames alone, so it does not format
    /// the row names it would not use.
    fn holds_boundary(events: &[LocatedEvent]) -> bool {
        let frames: Vec<TranscriptFrame> = events
            .iter()
            .flat_map(|e| Self::frames_of(&e.entry.event).into_iter().flatten())
            .collect();
        bounds_turn_in(&frames)
    }

    /// Render one window: the window's own events, folded against the events
    /// the lookback recovered. A window starting inside a turn reaches the fold
    /// with the turn's opening frame behind its first event, which is what
    /// keeps the summary row the fold derives at the frame that closed the
    /// turn. Only the window's events become rows.
    fn project_window(&self, window: &EventWindow) -> (Vec<TranscriptLine>, usize) {
        let ahead = if window.lines_start_offset > 0 {
            self.history
                .lookback_until(window.lines_start_offset, Self::holds_boundary)
        } else {
            Vec::new()
        };
        let (mut frames, mut names) = Self::named_frames(ahead.iter().map(|e| &e.entry));
        let start = frames.len();
        let (window_frames, window_names) =
            Self::named_frames(window.events.iter().map(|e| &e.entry));
        frames.extend(window_frames);
        names.extend(window_names);
        let lines = transcript_from_named_frames(&frames, &names, start..frames.len(), true);
        (lines, window.skipped)
    }
}

impl TranscriptSnapshot for SessionLogSnapshot {
    fn log_size(&self) -> u64 {
        self.history.log_size()
    }

    fn load(&self, max_bytes: u64) -> SnapshotLoad {
        let size = self.log_size();
        if size > max_bytes {
            return SnapshotLoad {
                lines: Vec::new(),
                skipped: 0,
                truncated: true,
            };
        }
        let read = self.history.read_whole_lenient();
        let lines = Self::project_events(&read.events);
        SnapshotLoad {
            lines,
            skipped: read.skipped,
            truncated: false,
        }
    }

    fn window(&self, anchor: u64, max_bytes: u64) -> WindowLoad {
        let window = self.history.window(anchor, max_bytes);
        let (lines, skipped) = self.project_window(&window);
        WindowLoad {
            lines,
            start_offset: window.lines_start_offset,
            next_offset: window.next_offset,
            skipped,
            bytes_total: window.bytes_total,
        }
    }

    fn tail_window(&self, max_bytes: u64) -> WindowLoad {
        let window = self.history.tail_window(max_bytes);
        let (lines, skipped) = self.project_window(&window);
        WindowLoad {
            lines,
            start_offset: window.lines_start_offset,
            next_offset: window.next_offset,
            skipped,
            bytes_total: window.bytes_total,
        }
    }

    fn window_before(&self, from_byte: u64, max_bytes: u64) -> WindowLoad {
        let window = self.history.window_before(from_byte, max_bytes);
        let (lines, skipped) = self.project_window(&window);
        WindowLoad {
            lines,
            start_offset: window.lines_start_offset,
            next_offset: window.next_offset,
            skipped,
            bytes_total: window.bytes_total,
        }
    }

    fn index_chunk(&self) -> IndexProgress {
        let progress = self.history.index_chunk();
        IndexProgress {
            indexed_bytes: progress.indexed_bytes,
            total_bytes: progress.total_bytes,
            done: progress.done,
        }
    }

    fn byte_at(&self, event_idx: usize) -> Option<u64> {
        self.history.byte_at(event_idx)
    }

    fn event_count(&self) -> Option<usize> {
        self.history.event_count()
    }
}

#[cfg(test)]
#[path = "session_log_snapshot_tests.rs"]
mod tests;
