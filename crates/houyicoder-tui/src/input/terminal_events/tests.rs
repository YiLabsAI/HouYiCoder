//! The reader's drain contract, driven without a terminal.

use super::*;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use houyicoder_platform::desktop::terminal::terminal_gone;
use std::io::Error;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::yield_now;
use std::time::Instant;

fn press(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

fn ctrl(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

/// A quiet channel drains at once rather than blocking.
///
/// This is the property the run loop depends on: the drain must always give
/// the loop back, because that is where the exit decision is made. A drain
/// that never returns is what let a session outlive its terminal.
#[test]
fn test_drain_returns_when_quiet() {
    let (_events, incoming) = channel::<Event>();
    let reader = EventReader::over(incoming);
    let started = Instant::now();
    let drained = reader.drain_with(|| false);
    assert!(
        drained.events.is_empty(),
        "a quiet channel reports no event"
    );
    assert!(!drained.gone, "a live terminal is not reported as gone");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the drain returned without waiting on a deadline"
    );
}

/// An event that arrives before the drain is handed over as it is.
#[test]
fn test_drain_forwards_event() {
    let (events, incoming) = channel::<Event>();
    let reader = EventReader::over(incoming);
    events.send(press('a')).expect("the reader is listening");
    let drained = reader.drain_with(|| false);
    assert!(
        matches!(
            drained.events.as_slice(),
            [Event::Key(key)] if key.code == KeyCode::Char('a')
        ),
        "the event reaches the loop unchanged"
    );
}

/// A poll that reports unready or interrupted retries rather than ending.
///
/// Ending here would drop the reader thread, leaving the session with no
/// keyboard and a channel that spins on immediate disconnects.
#[test]
fn test_read_retries_when_unready() {
    let mut attempts = 0;
    let event = read_event_with(
        || false,
        || {
            attempts += 1;
            match attempts {
                1 => Ok(false),
                2 => Err(Error::from(ErrorKind::Interrupted)),
                _ => Ok(true),
            }
        },
        || None,
        || Ok(true),
        || Ok(press('x')),
    );
    assert!(
        matches!(event, Some(Event::Key(key)) if key.code == KeyCode::Char('x')),
        "the reader retried quiet timeouts and returned the event"
    );
    assert_eq!(attempts, 3, "retried until ready");

    let mut polled = false;
    let hung = read_event_with(
        || true,
        || {
            polled = true;
            Ok(true)
        },
        || None,
        || Ok(true),
        || Ok(press('x')),
    );
    assert!(hung.is_none(), "hung up returns None");
    assert!(!polled, "did not poll after hangup");

    let err = read_event_with(
        || false,
        || Err(Error::from(ErrorKind::Other)),
        || None,
        || Ok(true),
        || Ok(press('x')),
    );
    assert!(err.is_none(), "an event source error returns None");

    let unready = read_event_with(
        || false,
        || Ok(true),
        || None,
        || Err(Error::from(ErrorKind::Other)),
        || Ok(press('x')),
    );
    assert!(unready.is_none(), "a readiness error returns None");
}

/// A control key the console reports in the form the event source cannot
/// resolve is taken before anything waits. Waiting first would hand that
/// record to an event source that drops it, losing the key for good.
#[test]
fn test_ctrl_key_before_wait() {
    let mut waits = 0;
    let mut reads = 0;
    let event = read_event_with(
        || false,
        || {
            waits += 1;
            Ok(true)
        },
        || Some(ctrl('o')),
        || Ok(true),
        || {
            reads += 1;
            Ok(press('x'))
        },
    );
    assert!(
        matches!(event, Some(Event::Key(key)) if key.code == KeyCode::Char('o')),
        "the control key reaches the loop"
    );
    assert_eq!(waits, 0, "the queued record was taken without a wait");
    assert_eq!(reads, 0, "the event source never read that record");
}

/// A control record that ends the wait is still in the queue, so the take
/// inspects it before the event source is asked for an event. This is the
/// case a user produces: the key arrives while the reader waits.
#[test]
fn test_ctrl_key_after_wait() {
    let mut takes = 0;
    let mut reads = 0;
    let event = read_event_with(
        || false,
        || Ok(true),
        || {
            takes += 1;
            (takes == 2).then(|| ctrl('o'))
        },
        || Ok(true),
        || {
            reads += 1;
            Ok(press('x'))
        },
    );
    assert!(
        matches!(event, Some(Event::Key(key)) if key.code == KeyCode::Char('o')),
        "the record that woke the wait reaches the loop"
    );
    assert_eq!(takes, 2, "the queue was inspected again after the wait");
    assert_eq!(reads, 0, "the event source never read that record");
}

/// An ordinary record is left to the event source: the take answers none and
/// the read returns the key that was actually typed.
#[test]
fn test_ordinary_record_reads() {
    let mut taken = 0;
    let event = read_event_with(
        || false,
        || Ok(true),
        || {
            taken += 1;
            None
        },
        || Ok(true),
        || Ok(press('q')),
    );
    assert!(
        matches!(event, Some(Event::Key(key)) if key.code == KeyCode::Char('q')),
        "the typed key reaches the loop"
    );
    assert_eq!(taken, 2, "the take ran before the wait and after it");
}

/// A ready poll whose read fails on a signal or a partial sequence retries
/// instead of ending the reader. Ending here on a transient blip would drop
/// the reader thread and leave the session with no keyboard.
#[test]
fn test_reader_survives_read_error() {
    let mut reads = 0;
    let event = read_event_with(
        || false,
        || Ok(true),
        || None,
        || Ok(true),
        || {
            reads += 1;
            match reads {
                1 => Err(Error::from(ErrorKind::Interrupted)),
                _ => Ok(press('z')),
            }
        },
    );
    assert!(
        matches!(event, Some(Event::Key(key)) if key.code == KeyCode::Char('z')),
        "a read that fails once must not end the reader"
    );
    assert_eq!(reads, 2, "the failed read was retried");
}

/// A reader whose thread is gone reports it rather than draining forever, so
/// a session can still be quit when its input source died.
#[test]
fn test_dead_reader_ends() {
    let (events, incoming) = channel::<Event>();
    drop(events);
    let reader = EventReader::over(incoming);
    assert!(
        reader.drain_with(|| false).gone,
        "a reader that is gone ends the session instead of reporting quiet"
    );
}

/// Every event the source yields reaches the loop, in order.
#[test]
fn test_forward_keeps_order() {
    let (events, incoming) = channel::<Event>();
    let queued = vec![press('a'), press('b'), press('c')];
    let mut remaining = queued.into_iter();
    forward(events, UiWake::new(), move || {
        remaining.next().map(|event| vec![event])
    });
    let seen: Vec<char> = incoming
        .try_iter()
        .map(|event| match event {
            Event::Key(key) => match key.code {
                KeyCode::Char(c) => c,
                _ => '?',
            },
            _ => '?',
        })
        .collect();
    assert_eq!(seen, ['a', 'b', 'c'], "no event is dropped or reordered");
}

/// A reader whose loop is gone stops reading instead of running on.
///
/// The loop exits while the reader is blocked in the terminal; the reader then
/// finds its channel closed. Stopping there is what keeps a finished session
/// from holding a thread on a terminal nobody reads.
#[test]
fn test_forward_stops_when_closed() {
    let (events, incoming) = channel::<Event>();
    drop(incoming);
    let yielded = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&yielded);
    forward(events, UiWake::new(), move || {
        let seen = counter.fetch_add(1, Ordering::Relaxed) + 1;
        (seen <= 3).then(|| vec![press('x')])
    });
    assert_eq!(
        yielded.load(Ordering::Relaxed),
        1,
        "the loop stopped at the first undeliverable event"
    );
}

/// Starting a reader wires a channel to the loop and runs the source on its
/// own thread, which is what keeps a closed terminal away from the loop. A
/// source that ends closes the channel, and the loop reads that as the end of
/// the session rather than as quiet.
#[test]
fn test_start_wires_a_reader() {
    let reader = start(|| None);
    let deadline = Instant::now() + Duration::from_secs(2);
    let gone = loop {
        if reader.drain_with(|| false).gone {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        yield_now();
    };
    assert!(gone, "a source that ended ends the session, not blocks it");
}

/// A reader whose lock was poisoned ends the session instead of panicking in
/// the loop: a session must stay quittable even after a panic touched it.
#[test]
fn test_poisoned_reader_ends() {
    let (_events, incoming) = channel::<Event>();
    let reader = EventReader::over(incoming);
    let poisoned = catch_unwind(AssertUnwindSafe(|| {
        let _held = reader.events.lock().expect("the lock is clean");
        panic!("a holder panicked, which poisons the lock");
    }));
    assert!(poisoned.is_err(), "the lock was poisoned");
    let drained = reader.drain_with(|| false);
    assert!(drained.events.is_empty(), "no event is invented");
    assert!(
        drained.gone,
        "a poisoned reader ends the session rather than panicking in the loop"
    );
}

/// The shared reader starts once and serves the loop from the process's own
/// terminal.
///
/// This is the only case that touches the real source: it starts the reader on
/// the test process's terminal and asks it for one drain. What matters is that
/// the call returns instead of blocking; the thread ends with the test process.
#[test]
fn test_shared_reader_starts() {
    let drained = EventReader::shared().drain();
    assert!(drained.events.is_empty(), "nothing typed during the run");
}

/// An empty drain that finds the terminal gone reports it, so the session
/// ends. The probe is a parameter because its answer only comes from a real
/// descriptor; what the loop does with that answer is what this test asserts.
#[test]
fn test_drain_reports_hangup() {
    let (_events, incoming) = channel::<Event>();
    let reader = EventReader::over(incoming);
    assert!(
        reader.drain_with(|| true).gone,
        "a hung-up terminal is reported, not mistaken for quiet"
    );
    assert!(
        !reader.drain_with(|| false).gone,
        "a quiet terminal stays quiet"
    );
}

/// The hangup probe answers on a descriptor that is not a terminal.
///
/// A test process has no terminal of its own, so the answer here is "no
/// hangup". What this pins is that the probe returns instead of blocking or
/// failing: a probe that panicked would end a session that is still fine.
#[test]
fn test_hangup_probe_answers() {
    assert!(
        !terminal_gone(),
        "a descriptor with no hangup is not reported as one"
    );
}

/// One drain takes every event the reader already holds, so a burst costs a
/// single frame instead of one frame per event.
#[test]
fn test_drain_takes_whole_burst() {
    let (events, incoming) = channel::<Event>();
    let reader = EventReader::over(incoming);
    for c in ['a', 'b', 'c'] {
        events.send(press(c)).expect("the reader is listening");
    }
    let drained = reader.drain();
    assert_eq!(drained.events.len(), 3, "every queued event is taken");
    assert!(!drained.gone, "the reader is still alive");
    assert!(
        reader.drain().events.is_empty(),
        "the channel is empty after one drain"
    );
}

/// A drain reports a reader that ended, so the loop leaves even when no wake
/// announced the close.
#[test]
fn test_drain_reports_closed() {
    let (events, incoming) = channel::<Event>();
    let reader = EventReader::over(incoming);
    events.send(press('a')).expect("the reader is listening");
    drop(events);
    let drained = reader.drain();
    assert_eq!(
        drained.events.len(),
        1,
        "events delivered before the close are kept"
    );
    assert!(drained.gone, "the closed reader ends the session");
}

/// The decoded fold chord maps to the event the run loop routes: the letter
/// carries the control modifier, so the chord a pipe-mode terminal sends
/// reaches the same dispatch a console keystroke does.
#[test]
fn test_pipe_ctrl_o_maps() {
    let event = vt_event(VtEvent::Ctrl('o')).expect("a control key maps");
    assert!(
        matches!(
            event,
            Event::Key(key)
                if key.code == KeyCode::Char('o')
                    && key.modifiers.contains(KeyModifiers::CONTROL)
        ),
        "the stream chord maps to the console chord: {event:?}"
    );
}

/// The decoded arrow keys keep their direction, and a paste marker carries
/// no event of its own: the text between is what the collector folds.
#[test]
fn test_pipe_arrows_map() {
    let up = vt_event(VtEvent::Arrow(true)).expect("an arrow maps");
    assert!(matches!(up, Event::Key(key) if key.code == KeyCode::Up));
    let down = vt_event(VtEvent::Arrow(false)).expect("an arrow maps");
    assert!(matches!(down, Event::Key(key) if key.code == KeyCode::Down));
    assert!(vt_event(VtEvent::PasteMark(true)).is_none());
}

/// The pipe reader decodes the bytes a pipe-mode terminal delivers into the
/// events the loop routes. One read returns one batch: the fold chord
/// arrives as its batch, and a sequence a chunk ends halfway through holds
/// its bytes for the read that completes it.
#[test]
fn test_pipe_read_decodes_events() {
    let fold = read_pipe_with(scripted(vec![vec![0x0f]])).expect("events from the chord batch");
    assert!(
        fold.iter().any(|event| {
            matches!(
                event,
                Event::Key(key)
                    if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL)
            )
        }),
        "the fold chord decodes: {fold:?}"
    );
}

/// A sequence split across reads completes on the read that brings its
/// last bytes: a read ending inside the escape sequence produces no event
/// and holds its bytes, and the reader continues rather than ending, so
/// the next read decodes the whole sequence at once.
#[test]
fn test_pipe_read_split_sequence() {
    // One queue shared across the reads the loop makes.
    let mut queue: Vec<Vec<u8>> = vec![b"A".to_vec(), b"\x1b[".to_vec()];
    let read = move |buffer: &mut [u8]| -> Result<usize, Error> {
        match queue.pop() {
            Some(chunk) => {
                buffer[..chunk.len()].copy_from_slice(&chunk);
                Ok(chunk.len())
            }
            None => Ok(0),
        }
    };
    let events = read_pipe_with(read).expect("the split sequence completes");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Key(key) if key.code == KeyCode::Up)),
        "the arrow the two reads carried together decodes: {events:?}"
    );
}

/// The read over a fixed list of chunks, ending after the list does. The
/// reader is called once per read, so a shared counter walks the list.
fn scripted(chunks: Vec<Vec<u8>>) -> impl FnMut(&mut [u8]) -> Result<usize, Error> {
    let mut queue = chunks;
    move |buffer: &mut [u8]| {
        if queue.is_empty() {
            return Ok(0);
        }
        let chunk = queue.remove(0);
        buffer[..chunk.len()].copy_from_slice(&chunk);
        Ok(chunk.len())
    }
}

/// A read that fails ends the reader: a pipe that errors is a source the
/// session can no longer be typed into, and the reader says so rather than
/// spinning on the error.
#[test]
fn test_pipe_read_error_ends() {
    let read = |_: &mut [u8]| -> Result<usize, Error> { Err(Error::other("pipe broken")) };
    assert!(
        read_pipe_with(read).is_none(),
        "a failed read ends the pipe reader"
    );
}

/// Every decoded event maps to the crossterm event the loop routes: the
/// full key set the decoder can produce, each with the right code.
#[test]
fn test_vt_event_maps_keys() {
    fn is_key(ev: Option<Event>, code: KeyCode) -> bool {
        matches!(ev, Some(Event::Key(k)) if k.code == code)
    }
    for (input, code) in [
        (VtEvent::Tab, KeyCode::Tab),
        (VtEvent::Left, KeyCode::Left),
        (VtEvent::Right, KeyCode::Right),
        (VtEvent::Home, KeyCode::Home),
        (VtEvent::End, KeyCode::End),
        (VtEvent::PageUp, KeyCode::PageUp),
        (VtEvent::PageDown, KeyCode::PageDown),
        (VtEvent::Delete, KeyCode::Delete),
        (VtEvent::BackTab, KeyCode::BackTab),
    ] {
        assert!(
            is_key(vt_event(input.clone()), code),
            "the key maps: {input:?}"
        );
    }
    assert!(
        matches!(vt_event(VtEvent::Paste(String::from("x"))), Some(Event::Paste(s)) if s == "x")
    );
    assert!(vt_event(VtEvent::PasteMark(true)).is_none());
}
