//! The reader's deadline, driven without a terminal.

use super::*;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
#[cfg(unix)]
use std::os::fd::AsFd;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

fn press(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}

/// A quiet channel answers on the deadline rather than blocking.
///
/// This is the property the run loop depends on: the wait must always give the
/// loop back, because that is where the exit decision is made. A wait that
/// never returns is what let a session outlive its terminal.
#[test]
fn test_reader_honors_deadline() {
    let (_events, incoming) = channel::<Event>();
    let reader = EventReader::over(incoming);
    let started = Instant::now();
    let input = reader.next_with(Duration::from_millis(50), || false);
    assert!(
        matches!(input, Input::Idle),
        "a quiet channel reports no input"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(40),
        "the wait used its deadline instead of returning at once"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the wait returned within its deadline"
    );
}

/// An event that arrives before the deadline is handed over as it is.
#[test]
fn test_reader_forwards_event() {
    let (events, incoming) = channel::<Event>();
    let reader = EventReader::over(incoming);
    events.send(press('a')).expect("the reader is listening");
    let input = reader.next_with(Duration::from_millis(50), || false);
    assert!(
        matches!(&input, Input::Event(Event::Key(key)) if key.code == KeyCode::Char('a')),
        "the event reaches the loop unchanged"
    );
}

/// A reader whose thread is gone reports no input rather than blocking, so a
/// session can still be quit when its input source died.
#[test]
fn test_dead_reader_idles() {
    let (events, incoming) = channel::<Event>();
    drop(events);
    let reader = EventReader::over(incoming);
    let input = reader.next_with(Duration::from_millis(50), || false);
    assert!(
        matches!(input, Input::Idle),
        "a closed channel reports no input instead of blocking"
    );
}

/// Every event the source yields reaches the loop, in order.
#[test]
fn test_forward_keeps_order() {
    let (events, incoming) = channel::<Event>();
    let script = vec![press('a'), press('b'), press('c')];
    let mut remaining = script.into_iter();
    forward(events, move || remaining.next());
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
    let yielded = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&yielded);
    forward(events, move || {
        let seen = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        (seen <= 3).then(|| press('x'))
    });
    assert_eq!(
        yielded.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the loop stopped at the first undeliverable event"
    );
}

/// Starting a reader wires a channel to the loop and runs the source on its
/// own thread, which is what keeps a closed terminal away from the loop.
#[test]
fn test_start_wires_a_reader() {
    let reader = start(|| None);
    assert!(
        matches!(
            reader.next_with(Duration::from_millis(50), || false),
            Input::Idle
        ),
        "a source that yields nothing leaves the loop idling, not blocked"
    );
}

/// A reader whose lock was poisoned reports no input instead of panicking in
/// the loop: a session must stay quittable even after a panic touched it.
#[test]
fn test_poisoned_reader_idles() {
    let (_events, incoming) = channel::<Event>();
    let reader = EventReader::over(incoming);
    let poisoned = catch_unwind(AssertUnwindSafe(|| {
        let _held = reader.events.lock().expect("the lock is clean");
        panic!("a holder panicked, which poisons the lock");
    }));
    assert!(poisoned.is_err(), "the lock was poisoned");
    assert!(
        matches!(reader.next(Duration::from_millis(10)), Input::Idle),
        "a poisoned reader idles rather than panicking in the loop"
    );
}

/// The shared reader starts once and serves the loop from the process's own
/// terminal.
///
/// This is the only case that touches the real source: it starts the reader on
/// the test process's terminal and asks it for one turn. Nothing types during
/// the run, so the turn idles; the thread ends with the test's process.
#[test]
fn test_shared_reader_starts() {
    let reader = EventReader::shared();
    // A test process has no terminal of its own, so either answer is right
    // here; what matters is that the call returns instead of blocking.
    assert!(
        matches!(
            reader.next(Duration::from_millis(10)),
            Input::Idle | Input::Gone
        ),
        "the shared reader answers its deadline"
    );
}

/// A deadline that finds the terminal gone reports it, so the loop ends.
///
/// The check is a parameter here because the answer only comes from a real
/// descriptor: what the loop does with the answer is the part worth pinning.
#[test]
fn test_deadline_reports_gone() {
    let (_events, incoming) = channel::<Event>();
    let reader = EventReader::over(incoming);
    assert!(
        matches!(
            reader.next_with(Duration::from_millis(10), || true),
            Input::Gone
        ),
        "a hung-up terminal is reported, not mistaken for quiet"
    );
    assert!(
        matches!(
            reader.next_with(Duration::from_millis(10), || false),
            Input::Idle
        ),
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
