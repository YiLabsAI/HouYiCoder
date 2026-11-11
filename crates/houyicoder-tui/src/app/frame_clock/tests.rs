use std::time::{Duration, Instant};

use super::{FRAME_INTERVAL, FrameClock};

/// Before any frame is drawn there is no spacing to honor, so the first frame
/// after silence is due immediately.
#[test]
fn test_clock_first_frame_due() {
    let clock = FrameClock::default();
    assert_eq!(clock.cooldown(Instant::now()), None);
}

/// A frame drawn a quarter of an interval ago is held back for the rest of the
/// interval rather than drawn again now.
#[test]
fn test_clock_holds_burst() {
    let start = Instant::now();
    let mut clock = FrameClock::default();
    clock.note_frame(start);
    let cooldown = clock
        .cooldown(start + FRAME_INTERVAL / 4)
        .expect("inside the frame window the next frame waits");
    assert_eq!(cooldown, FRAME_INTERVAL - FRAME_INTERVAL / 4);
}

/// Once the interval has elapsed the next frame is due, so a steady stream of
/// arrivals cannot starve the screen.
#[test]
fn test_clock_releases_after_interval() {
    let start = Instant::now();
    let mut clock = FrameClock::default();
    clock.note_frame(start);
    assert_eq!(clock.cooldown(start + FRAME_INTERVAL), None);
    assert_eq!(clock.cooldown(start + Duration::from_secs(1)), None);
}
