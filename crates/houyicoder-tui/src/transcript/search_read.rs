//! The search view's reads of the session log, and the read the view asked
//! for next.
//!
//! The search view reads the durable log on two occasions: entering a query
//! reads the log whole when it fits the size threshold and its newest window
//! when it does not, and stepping toward the older or newer match walks windows
//! until it reaches one holding a match. Both used to run on the key handler's
//! own path, so a large log froze the frame the reader pressed the key on. Both
//! run here now, where a walk checks its stop signal between windows.
//!
//! A read is requested rather than dispatched on the spot, because the budget
//! admits one read at a time and a request is how the second press is held
//! without starting a second walk. The request names the intent -- open, or
//! step toward older or newer -- and the byte coordinates are read from the
//! view when the read starts, so a request that waited behind a running walk
//! steps from where the view has arrived rather than from where it stood when
//! the key was pressed. Asking again replaces the stance that has not started,
//! which bounds a held-down step key at one running read plus one waiting.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::Sender;

use crate::transcript::read_lifecycle::{PendingRead, ReadLifecycle, ReadPoll, WorkerLease};
use crate::transcript::scroll::{SEARCH_LOG_THRESHOLD, WINDOW_MAX_BYTES, holds_matching_line};
use crate::transcript::snapshot::{SnapshotLoad, TranscriptSnapshot, WindowLoad};

/// How many windows a step walks before it gives up. The walk holds one window
/// at a time, so this bounds how far it travels, not what it holds. A walk that
/// spends it reports no step rather than claiming the log holds no older match:
/// the log is not proven to end here, the walk stopped at its own limit.
pub(crate) const SEARCH_WALK_MAX_WINDOWS: usize = 64;

/// Which way a step walks, and therefore which match of the window it lands on
/// the view focuses: the newest one for the older direction, the first for the
/// newer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkToward {
    Older,
    Newer,
}

/// What the search view is waiting for. The coordinates are not part of it:
/// they are read from the view when the read starts, so a request that
/// outlives the window it was made against still steps from the right place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchReadRequest {
    /// Read the log the way its size calls for at the tail, replacing the rows
    /// the view opened over with the log's own.
    Open,
    /// Walk windows toward the next match in this direction.
    Step(WalkToward),
}

/// One read of the session log, with the coordinates it was built against. The
/// same value is both what a worker walks and what the apply side re-checks,
/// because a result is only meaningful against the place it was read for: split
/// into two values, the two would have to be built from the same facts by two
/// call sites and kept in agreement.
#[derive(Debug, Clone)]
pub(crate) enum SearchReadJob {
    Open,
    /// Walk from the byte the current window begins at (older) or ends at
    /// (newer), toward the first window holding a match for the query.
    /// file_size is the log size frozen when the view opened, so events
    /// appended after that stay outside the walk.
    Step {
        from_byte: u64,
        toward: WalkToward,
        file_size: u64,
        query: String,
    },
}

/// What a read returned.
#[derive(Debug)]
pub(crate) enum SearchReadOutcome {
    /// The whole log as rendered rows, read under the size threshold.
    Under(SnapshotLoad),
    /// The newest window of a log over the threshold, with the log's byte size
    /// for the chrome's position readout.
    Over { log_bytes: u64, window: WindowLoad },
    /// A step landed on a window holding a match.
    Stepped(WindowLoad),
    /// A step reached the log's end, or spent its bound, or made no progress,
    /// without reaching a window that holds a match. Nothing is proven about
    /// the log beyond what the walk read, so the view stays where it is.
    NoStep,
    /// The log could not be read at all. The view keeps the rows it has.
    ReadFailed,
}

/// The read on file, as the apply side reads it.
pub(crate) type PendingSearchRead = PendingRead<SearchReadJob, SearchReadOutcome>;

/// What polling the read on file returned.
pub(crate) type SearchReadPoll = ReadPoll<SearchReadOutcome>;

impl SearchReadJob {
    /// Run the read and send its outcome, unless it was told to stop first. A
    /// stopped read sends nothing: the record that dispatched it is gone, so
    /// there is no one left to apply it.
    pub(crate) fn run_and_send(
        self,
        source: Arc<dyn TranscriptSnapshot>,
        tx: Sender<SearchReadOutcome>,
        stop: &AtomicBool,
    ) {
        if let Some(outcome) = self.run(&source, stop) {
            tx.send(outcome).ok();
        }
    }

    /// The read itself: None when the stop signal was raised before there was
    /// anything to report.
    fn run(
        self,
        source: &Arc<dyn TranscriptSnapshot>,
        stop: &AtomicBool,
    ) -> Option<SearchReadOutcome> {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        match self {
            Self::Open => Some(Self::read_open(source)),
            Self::Step {
                from_byte,
                toward,
                file_size,
                query,
            } => Self::read_step(source, from_byte, toward, file_size, &query, stop),
        }
    }

    /// Read the log the way its size calls for.
    fn read_open(source: &Arc<dyn TranscriptSnapshot>) -> SearchReadOutcome {
        let log_bytes = source.log_size();
        if log_bytes <= SEARCH_LOG_THRESHOLD {
            return SearchReadOutcome::Under(source.load(SEARCH_LOG_THRESHOLD));
        }
        match source.tail_window(WINDOW_MAX_BYTES) {
            Ok(window) => SearchReadOutcome::Over { log_bytes, window },
            Err(error) => {
                tracing::warn!("search tail window read failed: {error:?}");
                SearchReadOutcome::ReadFailed
            }
        }
    }

    /// Walk windows toward the direction, stopping at the first one that holds
    /// a match for the query.
    ///
    /// The match decision is the same matcher the view runs over the corpus it
    /// renders, so a landed window provably holds a match the view will also
    /// find: one matcher, not two kept in agreement.
    fn read_step(
        source: &Arc<dyn TranscriptSnapshot>,
        from_byte: u64,
        toward: WalkToward,
        file_size: u64,
        query: &str,
        stop: &AtomicBool,
    ) -> Option<SearchReadOutcome> {
        let mut cursor = from_byte;
        for _ in 0..SEARCH_WALK_MAX_WINDOWS {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            let read = match toward {
                WalkToward::Older => {
                    if cursor == 0 {
                        return Some(SearchReadOutcome::NoStep);
                    }
                    source.window_before(cursor, WINDOW_MAX_BYTES)
                }
                WalkToward::Newer => {
                    if cursor >= file_size {
                        return Some(SearchReadOutcome::NoStep);
                    }
                    Ok(source.window(cursor, WINDOW_MAX_BYTES))
                }
            };
            let window = match read {
                Ok(window) => window,
                Err(error) => {
                    tracing::warn!("search window read failed: {error:?}");
                    return Some(SearchReadOutcome::ReadFailed);
                }
            };
            let next = match toward {
                WalkToward::Older => window.start_offset,
                WalkToward::Newer => window.next_offset,
            };
            // An empty or corrupt window cannot be walked through, and a window
            // that does not move the cursor would walk the same bytes forever.
            if window.rows.is_empty() || next == cursor {
                return Some(SearchReadOutcome::NoStep);
            }
            if holds_matching_line(window.rows.lines(), query) {
                return Some(SearchReadOutcome::Stepped(window));
            }
            cursor = next;
        }
        Some(SearchReadOutcome::NoStep)
    }
}

/// The search view's reads: the shared lifecycle, plus the read the view is
/// waiting for.
///
/// The request lives here rather than on the view so that a view which closes
/// leaves nothing behind: one invalidation drops the read in flight and the
/// read not yet started together, and a landing after either is refused by the
/// generation.
#[derive(Debug, Default)]
pub(crate) struct SearchReads {
    reads: ReadLifecycle<SearchReadJob, SearchReadOutcome>,
    requested: Option<SearchReadRequest>,
}

impl SearchReads {
    /// Ask for a read. It starts on the next pump that finds room, which is
    /// this same call's own pump when no read is running.
    pub(crate) fn request(&mut self, request: SearchReadRequest) {
        self.requested = Some(request);
    }

    /// The read to start now, when there is room for one. None while a read is
    /// on file or a worker still holds the budget's only slot.
    pub(crate) fn take_request(&mut self) -> Option<SearchReadRequest> {
        if !self.reads.may_dispatch() {
            return None;
        }
        self.requested.take()
    }

    /// Drop the read in flight and the read requested, for a view that is no
    /// longer open. Nothing happens when neither was ever asked for, so a view
    /// closed for a long while does not move the generation on every pass.
    ///
    /// The view can close without the exit that would have invalidated: a stage
    /// fold closes it where it stands, and a walk left running would read the
    /// log for a reader who is no longer searching.
    pub(crate) fn forget_closed_view(&mut self) {
        if self.reads.holds_pending() || self.requested.is_some() {
            self.invalidate();
        }
    }

    /// Drop the read in flight and the read requested: the generation moves
    /// on, so neither can land in the view that follows.
    pub(crate) fn invalidate(&mut self) {
        self.reads.invalidate();
        self.requested = None;
    }

    /// The current generation, which a landing is checked against.
    pub(crate) fn epoch(&self) -> u64 {
        self.reads.epoch()
    }

    /// Whether a dispatched read is still on file, which is what a test reads
    /// to see the read the App would poll.
    #[cfg(test)]
    pub(crate) fn holds_pending(&self) -> bool {
        self.reads.holds_pending()
    }

    /// Record a dispatched read and receive the lease its worker holds.
    pub(crate) fn dispatch(
        &mut self,
        job: SearchReadJob,
        rx: mpsc::Receiver<SearchReadOutcome>,
    ) -> WorkerLease {
        self.reads.dispatch(job, rx)
    }

    /// Take the read on file out for polling.
    pub(crate) fn take(&mut self) -> Option<PendingSearchRead> {
        self.reads.take()
    }

    /// Put a still-pending read back on file.
    pub(crate) fn put_back(&mut self, read: PendingSearchRead) {
        self.reads.put_back(read);
    }
}

#[cfg(test)]
mod tests;
