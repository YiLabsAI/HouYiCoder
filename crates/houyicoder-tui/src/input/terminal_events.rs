//! One reader for terminal input, on a thread of its own.
//!
//! Waiting for a terminal event blocks inside the event reader, and when the
//! other end of the terminal closes the reader would spin instead of
//! returning. Reading on a separate thread keeps that from reaching the run
//! loop, which waits on a channel and always gets control back. The hangup
//! answer comes from the platform terminal boundary, so this module holds no
//! platform conditionals of its own.

use std::io::{self, ErrorKind};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::Builder;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use houyicoder_platform::desktop::terminal::console_ctrl_pending;
use houyicoder_platform::desktop::terminal::paste_arrives_as_keystrokes;
use houyicoder_platform::desktop::terminal::pipe_input;
use houyicoder_platform::desktop::terminal::take_console_ctrl_key;
use houyicoder_platform::desktop::terminal::terminal_gone;
use houyicoder_platform::desktop::terminal::vt::VtDecoder;
use houyicoder_platform::desktop::terminal::vt::VtEvent;
use houyicoder_platform::desktop::terminal::vt::read_pipe_bytes;
use houyicoder_platform::desktop::terminal::wait_console_input;
use houyicoder_platform::desktop::terminal::wait_consumes_record;

use crate::app::wake::{UiWake, ui_wake};
use crate::input::paste::burst::coalesce_with;

/// How long one event-source wait lasts before the reader checks the terminal
/// again. The run loop probes the descriptor on every drain, but that only
/// covers a loop that is already awake; the wait stays short so the reader
/// re-checks on its own rather than sleeping through a dead terminal.
const READ_WAIT: Duration = Duration::from_secs(2);

/// The process-wide reader. A session switch runs the run loop again, and
/// both runs must take events from the same reader: a second one would take
/// the keystrokes meant for the first.
static READER: OnceLock<EventReader> = OnceLock::new();

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
    /// The shared reader, started on first use. The source is the terminal
    /// itself: only a real terminal can answer it, which is why the loop it
    /// feeds is the part the tests drive.
    pub(crate) fn shared() -> &'static Self {
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
        self.drain_with(terminal_gone)
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
                // to read, and the loop probes it here on every empty drain.
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
/// One console record can carry a whole clipboard paste, so the read hands
/// back every event that run produced.
fn read_events() -> Option<Vec<Event>> {
    // A terminal that hands the process a pipe answers no console API, so
    // the bytes it delivers are decoded here instead of through the event
    // source that expects one.
    if pipe_input() {
        return read_pipe_events();
    }
    let event = read_event_with(
        terminal_gone,
        wait_for_input,
        || take_console_ctrl_key().and_then(vt_event),
        ready_to_read,
        event::read,
    )?;
    Some(coalesce_with(event, burst_pending, event::read))
}

/// Wait up to the read deadline for an event.
///
/// A host whose event source consumes a console record while it waits gets
/// the wait taken on the input queue itself, so the record that ends the
/// wait is still there for the control take to inspect. Every other host
/// waits inside its event source, which reports readiness without taking
/// anything.
fn wait_for_input() -> io::Result<bool> {
    if wait_consumes_record() {
        return wait_console_input(READ_WAIT);
    }
    event::poll(READ_WAIT)
}

/// Whether the event source can hand back an event now.
///
/// A host that consumed a record during its own wait has already decided the
/// record's fate, and its readiness answer means the event is held; the
/// source here is asked with a zero deadline, which consumes at most one
/// record, so a control record behind it is still inspected by the take. A
/// host whose wait only reported readiness already holds the event.
fn ready_to_read() -> io::Result<bool> {
    if wait_consumes_record() {
        return event::poll(Duration::ZERO);
    }
    Ok(true)
}

/// Read the byte stream a pipe-mode terminal delivers and decode it. The
/// read blocks until the pipe carries bytes, which is the same park the
/// console path takes inside its event source. The read is a parameter so
/// the decode path is drivable without a pipe: only the read itself needs
/// one.
fn read_pipe_events() -> Option<Vec<Event>> {
    read_pipe_with(read_pipe_bytes)
}

/// The pipe reader over a supplied read: bytes in, events out, until the
/// read ends.
fn read_pipe_with(mut read: impl FnMut(&mut [u8]) -> std::io::Result<usize>) -> Option<Vec<Event>> {
    let mut decoder = VtDecoder::default();
    let mut buffer = [0u8; 1024];
    loop {
        match read(&mut buffer) {
            Ok(0) => return None,
            Ok(n) => {
                let events: Vec<Event> = decoder
                    .feed(&buffer[..n])
                    .into_iter()
                    .filter_map(vt_event)
                    .collect();
                if !events.is_empty() {
                    return Some(events);
                }
                // A chunk that only held half a sequence reads on: the events
                // arrive when the sequence completes.
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                tracing::warn!(error = %e, "pipe read failed");
                return None;
            }
        }
    }
}

/// One decoded stream event as the event the loop routes. A paste bracket
/// opens a burst the collector folds; the events between are keystrokes.
pub(crate) fn vt_event(event: VtEvent) -> Option<Event> {
    let key = |code: KeyCode, modifiers: KeyModifiers| Event::Key(KeyEvent::new(code, modifiers));
    match event {
        VtEvent::Ctrl(letter) => Some(key(KeyCode::Char(letter), KeyModifiers::CONTROL)),
        VtEvent::Char(ch) => Some(key(KeyCode::Char(ch), KeyModifiers::NONE)),
        VtEvent::Enter => Some(key(KeyCode::Enter, KeyModifiers::NONE)),
        VtEvent::Backspace => Some(key(KeyCode::Backspace, KeyModifiers::NONE)),
        VtEvent::Tab => Some(key(KeyCode::Tab, KeyModifiers::NONE)),
        VtEvent::Esc => Some(key(KeyCode::Esc, KeyModifiers::NONE)),
        VtEvent::Arrow(up) => Some(key(
            if up { KeyCode::Up } else { KeyCode::Down },
            KeyModifiers::NONE,
        )),
        VtEvent::Left => Some(key(KeyCode::Left, KeyModifiers::NONE)),
        VtEvent::Right => Some(key(KeyCode::Right, KeyModifiers::NONE)),
        VtEvent::Home => Some(key(KeyCode::Home, KeyModifiers::NONE)),
        VtEvent::End => Some(key(KeyCode::End, KeyModifiers::NONE)),
        VtEvent::PageUp => Some(key(KeyCode::PageUp, KeyModifiers::NONE)),
        VtEvent::PageDown => Some(key(KeyCode::PageDown, KeyModifiers::NONE)),
        VtEvent::Delete => Some(key(KeyCode::Delete, KeyModifiers::NONE)),
        VtEvent::BackTab => Some(key(KeyCode::BackTab, KeyModifiers::NONE)),
        // The paste text is delivered as one event, so a newline inside it is
        // never mistaken for an Enter.
        VtEvent::Paste(text) => Some(Event::Paste(text)),
        // The paste markers are folded into the Paste event above.
        VtEvent::PasteMark(_) => None,
    }
}

/// Whether the console already holds another record. A typed key leaves the
/// queue empty; a paste leaves the next record waiting, which is what lets
/// the burst collector tell the two apart. A platform whose pastes arrive as
/// their own event never has a burst to collect.
fn burst_pending(window: Duration) -> bool {
    // Only the platform that delivers a paste as a run of keystrokes needs
    // to look ahead for the next record; a unix terminal reports its own
    // paste event, so the look-ahead would only delay a real keystroke.
    if !paste_arrives_as_keystrokes() {
        return false;
    }
    // A control record waiting at the head is not part of a paste, and the
    // look-ahead below would consume it into an event the source drops.
    // Stop the run here and leave the record for the reader to take.
    if console_ctrl_pending() {
        return false;
    }
    event::poll(window).unwrap_or(false)
}

/// Why a wait or poll ended without an event, when that is the ordinary quiet
/// state rather than a source that ended.
fn quiet(error: &io::Error) -> bool {
    matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock)
}

/// The event read loop, with the wait, the control take, the readiness check
/// and the reader supplied for tests.
fn read_event_with(
    hung_up: impl Fn() -> bool,
    mut wait: impl FnMut() -> io::Result<bool>,
    mut take_ctrl_key: impl FnMut() -> Option<Event>,
    mut ready: impl FnMut() -> io::Result<bool>,
    mut read: impl FnMut() -> io::Result<Event>,
) -> Option<Event> {
    loop {
        // A terminal that already hung up must not be read: the event source
        // answers that state by reading again, forever.
        if hung_up() {
            tracing::warn!("terminal hung up");
            return None;
        }
        // A control record already queued is taken before anything waits: a
        // wait would hand it to an event source that cannot resolve it, and
        // the record would be gone.
        if let Some(event) = take_ctrl_key() {
            return Some(event);
        }
        match wait() {
            Ok(true) => {}
            // A deadline that expired and a wait a signal interrupted both
            // mean no event yet. Returning here would end the reader.
            Ok(false) => continue,
            Err(e) if quiet(&e) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "terminal event wait failed");
                return None;
            }
        }
        // The record that ended the wait is still queued, so it is inspected
        // before the event source is asked for an event.
        if let Some(event) = take_ctrl_key() {
            return Some(event);
        }
        match ready() {
            // A ready poll does not promise a complete event. Treating a
            // partial read as the end of the source kills the reader thread
            // on a transient blip, and a session whose reader is gone can
            // never be typed into again.
            Ok(true) => match read() {
                Ok(event) => return Some(event),
                Err(_) => continue,
            },
            Ok(false) => continue,
            Err(e) if quiet(&e) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "terminal event poll failed");
                return None;
            }
        }
    }
}

/// Start the reader thread and hand back the channel it feeds. The source is
/// a parameter so the thread and the loop it wakes can be exercised without
/// a terminal: only the source itself needs one.
fn start(source: fn() -> Option<Vec<Event>>) -> EventReader {
    let (events, incoming) = channel();
    let signal = Arc::clone(ui_wake());
    let spawned = Builder::new()
        .name("terminal-input".to_string())
        .spawn(move || forward(events, signal, source));
    if let Err(e) = spawned {
        // Without a reader the loop never sees a keystroke. Say so rather
        // than leaving a session that silently ignores input.
        tracing::error!("terminal input reader did not start: {e}");
    }
    EventReader {
        events: Mutex::new(incoming),
    }
}

/// Forward every event the source yields until it yields none or the loop is
/// gone, waking the loop once per batch. The wake follows the send, so the
/// loop that wakes finds the event already in the channel; a source that
/// ends wakes the loop too, because the closed channel is how the loop
/// learns the session can no longer be typed into.
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
