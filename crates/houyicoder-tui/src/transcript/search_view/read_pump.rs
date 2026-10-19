//! The search view's background reads: start the read the view asked for,
//! apply what comes back, and drop both when the view closes.
//!
//! The reader's key press or command records a request; this side turns it
//! into a read when the shared budget has room, reads it inline where the
//! session is not connected, and applies the outcome only while the view that
//! asked is still the view on screen. The coordinates a step runs from are
//! read here at the moment the read starts, so a request that waited behind a
//! running walk steps from where the view has arrived rather than from where
//! it stood when the key was pressed.
//!
//! Entering the view no longer reads the log before the frame is drawn: the
//! view opens over the rows on hand, and this read replaces them with the log's
//! own when it lands. A log that cannot be read therefore costs the reader the
//! log's rows, not the search view.

use std::sync::mpsc;
use std::time::Duration;

use crate::app::state::App;
use crate::composition::shared_runtime;
use crate::notifications::{NotifKind, Notification};
use crate::transcript::search_read::{
    PendingSearchRead, SearchReadJob, SearchReadOutcome, SearchReadPoll, SearchReadRequest,
    WalkToward,
};

impl App {
    /// Advance the search view's read: apply a landed outcome, then start the
    /// requested read. Called from the loop body every pass, not inside the
    /// dirty gate, so an idle loop still picks the outcome up. Returns true
    /// when the screen changed, so the caller redraws.
    pub(crate) fn pump_search_read(&mut self) -> bool {
        if !self.ui.transcript_view.search_active() {
            // Closed without the exit that would have invalidated it, which a
            // stage fold does: nothing this read produces is wanted, and a walk
            // still inside the log is told to stop at its next window.
            self.sessions.current.search_reads.forget_closed_view();
            return false;
        }
        let mut landed = self.poll_search_read();
        if self.dispatch_search_read() {
            // Only a read that ran where it stood has its outcome in hand
            // already. A read on a worker answers on a later pass, and looking
            // for it here would let its outcome land inside the pass that
            // started it.
            landed |= self.poll_search_read();
        }
        landed
    }

    /// Apply the read on file when it has landed, or leave it for a later pass.
    fn poll_search_read(&mut self) -> bool {
        let Some(pending) = self.sessions.current.search_reads.take() else {
            return false;
        };
        match pending.poll() {
            SearchReadPoll::Pending => {
                self.sessions.current.search_reads.put_back(pending);
                false
            }
            SearchReadPoll::Ready(outcome) => {
                self.apply_search_read(outcome, &pending);
                true
            }
            SearchReadPoll::Disconnected => {
                // The worker died or dropped its sender without sending, so the
                // step the reader asked for will not arrive. The record is off
                // file already; the reader is told rather than left waiting for
                // a window that never comes.
                tracing::warn!("search read worker left without an outcome");
                self.notice_search_read_failed();
                true
            }
        }
    }

    /// Start the read the view asked for, when there is room for one. Returns
    /// whether it ran where it stood, so its outcome is already in hand; false
    /// when no read started, or when the read went to a worker and answers on a
    /// later pass.
    fn dispatch_search_read(&mut self) -> bool {
        let Some(request) = self.sessions.current.search_reads.take_request() else {
            return false;
        };
        // No source and no view to step from: the request is dropped rather
        // than left to be examined on every pass for a read that cannot start.
        let Some(source) = self.sessions.persistence.snapshot.clone() else {
            return false;
        };
        let Some(job) = self.search_job_for(request) else {
            return false;
        };
        // The job goes to the worker and its copy stays on file as the stamp,
        // so the outcome can be checked against the place it was read for.
        let (tx, rx) = mpsc::channel();
        let lease = self.sessions.current.search_reads.dispatch(job.clone(), rx);
        if self.sessions.current.connection.is_some() {
            shared_runtime().spawn_blocking(move || job.run_and_send(source, tx, lease.stop()));
            false
        } else {
            // A source with no connection is not a live session's log: the
            // compositions that build a session set the source and the
            // connection together, and nothing clears the connection afterwards,
            // so the composition that reaches this is one the tests build. The
            // read runs where it stands there, and no log whose walk could hold
            // a frame is read this way.
            job.run_and_send(source, tx, lease.stop());
            true
        }
    }

    /// The read for a request, built from where the view stands now.
    fn search_job_for(&self, request: SearchReadRequest) -> Option<SearchReadJob> {
        match request {
            SearchReadRequest::Open => Some(SearchReadJob::Open),
            SearchReadRequest::Step(toward) => {
                // A step walks window against window, so it needs the view to
                // still be browsing one.
                if !self.window_browsing() {
                    return None;
                }
                let from_byte = match toward {
                    WalkToward::Older => self.window_anchor(),
                    WalkToward::Newer => self.window_end(),
                };
                Some(SearchReadJob::Step {
                    from_byte,
                    toward,
                    file_size: self.frozen_file_size(),
                    query: self.ui.transcript_view.search_query().to_string(),
                })
            }
        }
    }

    /// Apply a landed read to the view it was read for, or drop it when that
    /// view has moved on. Returns whether the view changed.
    fn apply_search_read(&mut self, outcome: SearchReadOutcome, read: &PendingSearchRead) {
        if !self.ui.transcript_view.search_active() {
            tracing::debug!("search read dropped: the view is gone");
            return;
        }
        if read.epoch != self.sessions.current.search_reads.epoch() {
            tracing::debug!(
                read_epoch = read.epoch,
                current = self.sessions.current.search_reads.epoch(),
                "search read dropped: the view was entered again"
            );
            return;
        }
        match outcome {
            SearchReadOutcome::Under(load) => {
                self.ui
                    .transcript_view
                    .land_search_under(load.rows, load.truncated, load.skipped);
                self.focus_newest_search_match();
            }
            SearchReadOutcome::Over { log_bytes, window } => {
                self.ui.transcript_view.land_search_over_window(
                    window.rows,
                    log_bytes,
                    window.skipped,
                    window.start_offset,
                    window.next_offset,
                );
                self.focus_newest_search_match();
            }
            SearchReadOutcome::Stepped(window) => {
                let SearchReadJob::Step {
                    from_byte,
                    toward,
                    query,
                    ..
                } = &read.stamp
                else {
                    tracing::warn!("a stepped outcome arrived for a read that was not a step");
                    return;
                };
                // The window the walk started from has to be the window on
                // screen still, and the query the same: a landed window chosen
                // for one place or one query would replace the corpus with rows
                // nobody asked for.
                if self.ui.transcript_view.search_query() != query.as_str() {
                    tracing::debug!("search step dropped: the query changed while it walked");
                    return;
                }
                let standing_at = match toward {
                    WalkToward::Older => self.window_anchor(),
                    WalkToward::Newer => self.window_end(),
                };
                if standing_at != *from_byte {
                    tracing::debug!(
                        from_byte = *from_byte,
                        standing_at,
                        "search step dropped: the window moved while it walked"
                    );
                    return;
                }
                self.ui.transcript_view.replace_search_window(
                    window.rows,
                    window.start_offset,
                    window.next_offset,
                    window.skipped,
                );
                // The landed window holds a match for the query, since the walk
                // decided it with the matcher the view runs. Which match to
                // focus follows the direction walked: the newest one for the
                // older walk, the first for the newer.
                match toward {
                    WalkToward::Older => self.ui.transcript_view.search_focus_last(),
                    WalkToward::Newer => self.ui.transcript_view.search_focus_first(),
                }
                self.jump_to_focused_match();
            }
            SearchReadOutcome::NoStep => {
                // The walk reached the log's end or spent its bound without a
                // match. The view stays where it is; the chrome's partial total
                // already says the walk did not cover the whole log.
                tracing::debug!("search step found no window to land on");
            }
            SearchReadOutcome::ReadFailed => {
                tracing::warn!("search read failed; the view keeps the rows it has");
                self.notice_search_read_failed();
            }
        }
    }

    /// Focus the newest match of the corpus just landed, if there is one.
    fn focus_newest_search_match(&mut self) {
        if self.ui.transcript_view.search_focus_newest() {
            self.jump_to_focused_match();
        }
    }

    /// Tell the reader a search read came back empty-handed.
    fn notice_search_read_failed(&mut self) {
        self.ui.notifications.add(Notification::immediate(
            "search-read-failed",
            NotifKind::text("session log unreadable — search keeps its rows"),
            Duration::from_millis(4000),
        ));
    }
}

#[cfg(test)]
mod tests;
