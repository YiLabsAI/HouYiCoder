//! The session-log implementation of the TUI's TranscriptSnapshot port: the
//! search view reads the durable session log through the shared history
//! reader, maps each event to the frames the live stream carries, and folds
//! them with the same turn fold the live render uses, so scrolling
//! through history shows the turns the live view showed. Each frame carries
//! the durable identity of the event that produced it, and the row names are
//! derived through the same function the live path uses, so one event names
//! one row in every read, whatever window it started from.
//!
//! The mappings are the service layer's map_session_update and
//! map_acpx_notification, the same two the live push uses. A local copy would
//! drift, as one already did when it left out the interruption notice.
//!
//! Run state is what the two paths cannot share: this one reads a log written
//! by another process, so a turn that no record closes stays open here.
//!
//! The byte windows, the sidecar offset queries, and the turn lookback belong
//! to SessionHistory; this type only renders what that reader returns. The G
//! key in the search view triggers a full epoch scan with progress, which
//! gives the sidecar index its one check and rebuild.

use std::sync::Arc;

#[cfg(test)]
use houyicoder_api::session::SessionLog;
#[cfg(test)]
use houyicoder_context::SessionId;
use houyicoder_context::{ContextError, SessionEvent, SessionLogEntry};
use houyicoder_protocol::envelope::DurableEventId;
use houyicoder_service::protocol_adapter::{map_acpx_notification, map_session_update};
use houyicoder_tui::records::TranscriptLine;
use houyicoder_tui::row_id::RowId;
use houyicoder_tui::transcript::NamedRows;
use houyicoder_tui::transcript::snapshot::{
    IndexProgress, ReadError, SnapshotLoad, TranscriptSnapshot, WindowLoad,
};
use houyicoder_tui::transcript::{TranscriptFrame, bounds_turn_in, transcript_from_named_frames};

use crate::session::history::{EventWindow, LocatedEvent, SessionHistory};

/// The window read budget: 256 KB per screen (~70 events at 3.6 KB avg).
#[cfg(test)]
const WINDOW_MAX_BYTES: u64 = 256 * 1024;

/// The session-log implementation of the TranscriptSnapshot port: holds the
/// shared history reader and renders its windows to lines.
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
    /// route to the log's sidecar index instead of each walking the log on its
    /// own.
    pub fn with_history(history: Arc<SessionHistory>) -> Self {
        Self { history }
    }

    /// Map a durable event to the frames the fold reads, through the same two
    /// functions the live push uses: the session/update stream and the acpx
    /// notifications it has no variant for. Both are needed — a run completion
    /// record arrives as a notification, and it is what tells the fold where a
    /// turn ended.
    fn frames_of(event: &SessionEvent) -> [Option<TranscriptFrame>; 2] {
        [
            map_session_update(event).map(TranscriptFrame::Session),
            map_acpx_notification(event).map(TranscriptFrame::Acpx),
        ]
    }

    /// The frames a run of durable events renders to, along with the row
    /// identity each frame carries. The identity names both the event that
    /// produced the frame and the frame's slot in the event's canonical frame
    /// order, so the two frames of one event stay distinguishable and a turn's
    /// summary row is named by the event the log recorded the close in — the
    /// same name in every read, whatever window it started from. The
    /// derivation is the one the live read path calls, so the two readers
    /// cannot drift apart.
    fn named_frames<'a>(
        events: impl IntoIterator<Item = &'a SessionLogEntry>,
    ) -> (Vec<TranscriptFrame>, Vec<RowId>) {
        let mut frames = Vec::new();
        let mut names = Vec::new();
        for event in events {
            let id = DurableEventId(event.id.to_string());
            for frame in Self::frames_of(&event.event).into_iter().flatten() {
                // Both functions yield log categories, so every frame here
                // comes from the entry and the position argument is never
                // read: no frontend-raised frame reaches this loop.
                names.push(RowId::of(&frame, Some(&id), 0));
                frames.push(frame);
            }
        }
        (frames, names)
    }

    /// Render a run of durable events to transcript lines. The snapshot has
    /// no run state to consult, so a turn its log carries no record for is
    /// left open: the snapshot never claims a turn ended that the log does
    /// not record as ended.
    fn render_events<'a>(
        events: impl IntoIterator<Item = &'a SessionLogEntry>,
    ) -> Vec<TranscriptLine> {
        Self::render_named(events).lines
    }

    /// Render a run of durable events to rows with their identities, the
    /// shape a window read hands the TUI.
    fn render_named<'a>(events: impl IntoIterator<Item = &'a SessionLogEntry>) -> NamedRows {
        let (frames, names) = Self::named_frames(events);
        transcript_from_named_frames(&frames, &names, 0..frames.len(), true)
    }

    /// Whether a step of events holds where a turn begins or ends, in the
    /// sense the fold reads: a message that opened a turn, or the record
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
    fn render_window(&self, window: &EventWindow) -> (NamedRows, usize) {
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
        let rows = transcript_from_named_frames(&frames, &names, start..frames.len(), true);
        (rows, window.skipped)
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
        let lines = Self::render_events(&read.events);
        SnapshotLoad {
            lines,
            skipped: read.skipped,
            truncated: false,
        }
    }

    fn window(&self, anchor: u64, max_bytes: u64) -> WindowLoad {
        let window = self.history.window(anchor, max_bytes);
        let (rows, skipped) = self.render_window(&window);
        WindowLoad {
            lines: rows.lines,
            row_ids: rows.ids,
            start_offset: window.lines_start_offset,
            next_offset: window.next_offset,
            skipped,
            bytes_total: window.bytes_total,
        }
    }

    fn tail_window(&self, max_bytes: u64) -> Result<WindowLoad, ReadError> {
        match self.history.tail_window(max_bytes) {
            Ok(window) => {
                let (rows, skipped) = self.render_window(&window);
                Ok(WindowLoad {
                    lines: rows.lines,
                    row_ids: rows.ids,
                    start_offset: window.lines_start_offset,
                    next_offset: window.next_offset,
                    skipped,
                    bytes_total: window.bytes_total,
                })
            }
            Err(ContextError::Io) => Err(ReadError::Io),
            Err(other) => {
                // No window to render and nothing to retry.
                tracing::warn!("tail window unavailable: {other}");
                Ok(WindowLoad::default())
            }
        }
    }

    fn window_before(&self, from_byte: u64, max_bytes: u64) -> Result<WindowLoad, ReadError> {
        match self.history.window_before(from_byte, max_bytes) {
            Ok(window) => {
                let (rows, skipped) = self.render_window(&window);
                Ok(WindowLoad {
                    lines: rows.lines,
                    row_ids: rows.ids,
                    start_offset: window.lines_start_offset,
                    next_offset: window.next_offset,
                    skipped,
                    bytes_total: window.bytes_total,
                })
            }
            Err(ContextError::Io) => Err(ReadError::Io),
            Err(other) => {
                tracing::warn!("window-before unavailable: {other}");
                Ok(WindowLoad::default())
            }
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
mod tests;
