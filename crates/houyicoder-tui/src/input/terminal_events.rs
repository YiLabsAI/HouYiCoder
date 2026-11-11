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

#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::IsTerminal;
#[cfg(unix)]
use std::io::stdin;
use std::io::{self, ErrorKind};
#[cfg(unix)]
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::Builder;
use std::time::Duration;

use crossterm::event::{self, Event};
#[cfg(unix)]
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

use crate::app::wake::{UiWake, ui_wake};
use crate::input::paste::burst::coalesce_with;

/// How long one event-source wait lasts before the reader checks the terminal
/// again.
///
/// The reader is parked inside the event source for this long, so it cannot
/// look at the terminal while it waits. The run loop probes the descriptor on
/// every drain and ends the session on a hangup, but that only covers a loop
/// that is already awake. The wait stays short so the reader re-checks the
/// descriptor on its own at a human-scale cadence rather than sleeping through
/// a dead terminal.
const READ_WAIT: Duration = Duration::from_secs(2);

/// The process-wide reader.
///
/// A session switch runs the run loop again, and both runs must take events
/// from the same reader: a second one would take the keystrokes meant for the
/// first.
static READER: OnceLock<EventReader> = OnceLock::new();

/// The terminal descriptor the event reader actually reads.
///
/// The reader takes standard input when that is a terminal and the controlling
/// terminal otherwise, so a process fed by a pipe still reads a terminal. The
/// hangup check has to watch the same descriptor: a pipe whose writer exited is
/// end-of-file, not a dead terminal, and a terminal that hung up behind a pipe
/// would otherwise go unnoticed.
#[cfg(unix)]
fn terminal_hung_up() -> bool {
    probe(watched_terminal())
}

/// The terminal the event reader reads, or None when the process has none.
///
/// The reader takes standard input when that is a terminal and the controlling
/// terminal otherwise, so a process fed by a pipe still reads a terminal. A
/// pipe whose writer exited is end-of-file, not a dead terminal, which is why
/// standard input is only watched when it is the descriptor in use.
#[cfg(unix)]
fn watched_terminal() -> Option<BorrowedFd<'static>> {
    static WATCHED: OnceLock<Option<OwnedFd>> = OnceLock::new();
    if stdin().is_terminal() {
        return None;
    }
    WATCHED
        .get_or_init(|| {
            File::options()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .ok()
                .map(OwnedFd::from)
        })
        .as_ref()
        .map(|tty| tty.as_fd())
}

/// Whether the watched terminal reported a hangup.
///
/// A closed terminal leaves its descriptor readable with nothing to read, which
/// the event reader answers by reading again. Asking the descriptor directly is
/// the only check that does not depend on a signal arriving.
#[cfg(unix)]
fn probe(tty: Option<BorrowedFd<'_>>) -> bool {
    let input = stdin();
    let fd = match &tty {
        Some(tty) => tty.as_fd(),
        None if input.is_terminal() => input.as_fd(),
        None => return false,
    };
    let mut fds = [PollFd::new(
        fd,
        PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR,
    )];
    poll(&mut fds, PollTimeout::ZERO).is_ok()
        && fds[0]
            .revents()
            .is_some_and(|flags| flags.intersects(PollFlags::POLLHUP | PollFlags::POLLERR))
}

/// A platform whose console does not report a hangup through a descriptor keeps
/// the same shape; its close event arrives through the console API instead.
#[cfg(not(unix))]
fn terminal_hung_up() -> bool {
    false
}

/// The channel a reader thread feeds and the run loop drains.
pub(crate) struct EventReader {
    /// A receiver may not be shared, and the reader is process-wide. The lock
    /// is held across the wait on purpose: there is exactly one consumer, and
    /// serializing it keeps two runs of the loop from splitting one keystroke
    /// stream between them.
    events: Mutex<Receiver<Event>>,
}

/// Every event one non-blocking drain found.
pub(crate) struct Drained {
    /// The events the channel held, in arrival order.
    pub(crate) events: Vec<Event>,
    /// The reader is gone: no event will arrive again.
    pub(crate) gone: bool,
}

impl EventReader {
    /// The shared reader, started on first use.
    pub(crate) fn shared() -> &'static Self {
        // The source is the terminal itself: a bounded wait, then the event the
        // readiness promised. Only a real terminal can answer it, which is why
        // the loop it feeds is the part the tests drive.
        READER.get_or_init(|| start(read_events))
    }

    /// A reader over a channel the caller owns, for tests that drive the
    /// deadline without a terminal.
    #[cfg(test)]
    pub(crate) fn over(events: Receiver<Event>) -> Self {
        Self {
            events: Mutex::new(events),
        }
    }

    /// Take every event the reader already delivered, without blocking. The
    /// loop calls this once per wake, so a burst of arrivals costs one frame
    /// rather than one frame each.
    pub(crate) fn drain(&self) -> Drained {
        self.drain_with(terminal_hung_up)
    }

    /// The drain, with the hangup probe supplied so both answers it can give
    /// are driven without a terminal.
    pub(crate) fn drain_with(&self, hung_up: fn() -> bool) -> Drained {
        let Ok(events) = self.events.lock() else {
            // A poisoned lock means a panic while consuming. The receiver's
            // queue state is unknown, so the session ends through its normal
            // teardown instead of running with input nobody can read.
            return Drained {
                events: Vec::new(),
                gone: true,
            };
        };
        let mut drained = Vec::new();
        loop {
            match events.try_recv() {
                Ok(event) => drained.push(event),
                // An empty queue is exactly the state in which the reader is
                // parked inside the event source. A terminal that hung up
                // while it waits leaves the descriptor readable with nothing
                // to read, so the reader never returns and never notices. The
                // descriptor is the only place that state is visible, and the
                // loop probes it here on every empty drain.
                Err(TryRecvError::Empty) => {
                    return Drained {
                        events: drained,
                        gone: hung_up(),
                    };
                }
                Err(TryRecvError::Disconnected) => {
                    return Drained {
                        events: drained,
                        gone: true,
                    };
                }
            }
        }
    }
}

/// Read one terminal event, retrying quiet timeouts and signal interruptions.
///
/// One console record can carry a whole clipboard paste, so the read hands
/// back every event that run produced.
fn read_events() -> Option<Vec<Event>> {
    let event = read_event_with(terminal_hung_up, || event::poll(READ_WAIT), event::read)?;
    Some(coalesce_with(event, burst_pending, event::read))
}

/// Whether the console already holds another record. A typed key leaves the
/// queue empty; a paste leaves the next record waiting, which is what lets the
/// burst collector tell the two apart.
#[cfg(windows)]
fn burst_pending(window: Duration) -> bool {
    event::poll(window).unwrap_or(false)
}

/// A platform that reports a paste as its own event has no burst to collect.
#[cfg(not(windows))]
fn burst_pending(_window: Duration) -> bool {
    false
}

/// The event read loop, with the readiness poll and reader supplied for tests.
fn read_event_with(
    hung_up: impl Fn() -> bool,
    mut poll: impl FnMut() -> io::Result<bool>,
    mut read: impl FnMut() -> io::Result<Event>,
) -> Option<Event> {
    loop {
        // A terminal that already hung up must not be read: the event source
        // answers that state by reading again, forever.
        if hung_up() {
            tracing::warn!("terminal hung up");
            return None;
        }
        match poll() {
            // A ready poll does not promise a complete event. The reader can
            // still be interrupted by a signal or meet a partial escape
            // sequence. Treating either as the end of the source kills the
            // reader thread on a transient blip, and a session whose reader
            // is gone can never be typed into again. The hangup check at the
            // top of the loop is the only real end; a read that fails while
            // the terminal is still live is retried.
            Ok(true) => match read() {
                Ok(event) => return Some(event),
                Err(_) => continue,
            },
            // A deadline that expired and a wait a signal interrupted both
            // mean no event yet. Returning here would end the reader, and a
            // session whose reader is gone can never be typed into again.
            Ok(false) => continue,
            Err(e) if e.kind() == ErrorKind::Interrupted || e.kind() == ErrorKind::WouldBlock => {
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "terminal event poll failed");
                return None;
            }
        }
    }
}

/// Start the reader thread and hand back the channel it feeds.
///
/// The source is a parameter so the thread and the loop it wakes can be
/// exercised without a terminal: only the source itself needs one.
fn start(source: fn() -> Option<Vec<Event>>) -> EventReader {
    let (events, incoming) = channel();
    let signal = Arc::clone(ui_wake());
    let spawned = Builder::new()
        .name("terminal-input".to_string())
        .spawn(move || forward(events, signal, source));
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
/// gone, waking the loop once per batch.
///
/// The wake follows the send, so the loop that wakes finds the event already
/// in the channel. A source that ends wakes the loop too, because the closed
/// channel is how the loop learns the session can no longer be typed into.
///
/// The source is a parameter so the channel behaviour can be driven without a
/// terminal: the loop is the part that must not lose an event or outlive its
/// receiver.
fn forward(
    events: Sender<Event>,
    signal: Arc<UiWake>,
    mut source: impl FnMut() -> Option<Vec<Event>>,
) {
    while let Some(batch) = source() {
        let mut sent = false;
        for event in batch {
            if events.send(event).is_err() {
                return;
            }
            sent = true;
        }
        if sent {
            signal.wake();
        }
    }
    signal.wake();
}

#[cfg(test)]
mod tests;
