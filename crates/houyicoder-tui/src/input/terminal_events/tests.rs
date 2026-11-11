//! The reader's drain contract, driven without a terminal.

use super::*;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::io::Error;
#[cfg(unix)]
use std::os::fd::AsFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::yield_now;
use std::time::Instant;

fn press(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
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
                2 => Err(io::Error::from(ErrorKind::Interrupted)),
                _ => Ok(true),
            }
        },
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
        || Ok(press('x')),
    );
    assert!(hung.is_none(), "hung up returns None");
    assert!(!polled, "did not poll after hangup");

    let err = read_event_with(
        || false,
        || Err(io::Error::from(ErrorKind::Other)),
        || Ok(press('x')),
    );
    assert!(err.is_none(), "other error returns None");
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
        !terminal_hung_up(),
        "a descriptor with no hangup is not reported as one"
    );
}

/// A descriptor that is open and quiet is not a hangup.
///
/// This drives the probe over a descriptor the test owns, so the answer a
/// closed terminal gives is decided by the same code the session uses rather
/// than by the test process's own terminal.
#[cfg(unix)]
#[test]
fn test_probe_reports_live_descriptor() {
    let quiet = std::fs::File::open("/dev/null").expect("open a quiet descriptor");
    assert!(
        !probe(Some(quiet.as_fd())),
        "a quiet descriptor must not end the session"
    );
    assert!(
        !probe(None),
        "no descriptor to watch is not a hangup either"
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
