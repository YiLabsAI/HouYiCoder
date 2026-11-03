//! One reader for terminal input, on a thread of its own.
//!
//! Waiting for a terminal event blocks inside the event reader. When the other
//! end of the terminal closes, the descriptor stays readable and every read
//! returns end-of-file, so the reader keeps reading instead of returning: the
//! caller never regains control, and a session whose terminal has gone away
//! spins at a full core instead of ending.
//!
//! Reading on a separate thread keeps that from reaching the run loop. The loop
//! waits on a channel with a deadline, so it always gets control back to answer
//! the one question it owns: is this session still supposed to be running.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::thread::Builder;
use std::time::Duration;

use crossterm::event::{self, Event};

/// How long the reader waits before looking at the channel again.
///
/// The wait is long because the reader has nothing else to do: an event is what
/// it exists for. It is finite so the loop can notice a closed channel and
/// leave rather than blocking on a thread that will never send again.
const READ_WAIT: Duration = Duration::from_secs(3600);

/// The process-wide reader.
///
/// A session switch runs the run loop again, and both runs must take events
/// from the same reader: a second one would take the keystrokes meant for the
/// first.
static READER: OnceLock<EventReader> = OnceLock::new();

/// What one turn of the run loop found.
pub(crate) enum Input {
    /// One event arrived.
    Event(Event),
    /// Nothing arrived before the deadline, or the reader is gone. The loop
    /// keeps its cadence either way; a session with no reader is unusable for
    /// typing but can still be quit, which is the safe direction.
    Idle,
}

/// The channel a reader thread feeds and the run loop drains.
pub(crate) struct EventReader {
    /// A receiver may not be shared, and the reader is process-wide. The lock
    /// is held across the wait on purpose: there is exactly one consumer, and
    /// serializing it keeps two runs of the loop from splitting one keystroke
    /// stream between them.
    events: Mutex<Receiver<Event>>,
}

impl EventReader {
    /// The shared reader, started on first use.
    pub(crate) fn shared() -> &'static Self {
        // The source is the terminal itself: a bounded wait, then the event the
        // readiness promised. Only a real terminal can answer it, which is why
        // the loop it feeds is the part the tests drive.
        READER.get_or_init(|| {
            start(|| {
                event::poll(READ_WAIT).ok().filter(|ready| *ready)?;
                event::read().ok()
            })
        })
    }

    /// A reader over a channel the caller owns, for tests that drive the
    /// deadline without a terminal.
    #[cfg(test)]
    pub(crate) fn over(events: Receiver<Event>) -> Self {
        Self {
            events: Mutex::new(events),
        }
    }

    /// Wait up to the deadline for the next event.
    pub(crate) fn next(&self, deadline: Duration) -> Input {
        let Ok(events) = self.events.lock() else {
            return Input::Idle;
        };
        match events.recv_timeout(deadline) {
            Ok(event) => Input::Event(event),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Input::Idle,
        }
    }
}

/// Start the reader thread and hand back the channel it feeds.
///
/// The source is a parameter so the wiring can be driven without a terminal:
/// the thread, the channel, and the loop are one unit, and only the source
/// itself needs a real terminal.
fn start(source: fn() -> Option<Event>) -> EventReader {
    let (events, incoming) = channel();
    let spawned = Builder::new()
        .name("terminal-input".to_string())
        .spawn(move || forward(events, source));
    if let Err(e) = spawned {
        // Without a reader the loop never sees a keystroke. Say so rather than
        // leaving a session that silently ignores input.
        tracing::error!("terminal input reader did not start: {e}");
    }
    EventReader {
        events: Mutex::new(incoming),
    }
}

/// Forward every event the source yields until it yields none or the loop is
/// gone.
///
/// The source is a parameter so the channel behaviour can be driven without a
/// terminal: the loop is the part that must not lose an event or outlive its
/// receiver.
fn forward(events: Sender<Event>, mut source: impl FnMut() -> Option<Event>) {
    while let Some(event) = source() {
        if events.send(event).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests;
