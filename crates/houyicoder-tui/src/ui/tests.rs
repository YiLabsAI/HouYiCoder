//! The published-frame contract: what a reader sees while a draw runs and
//! after one fails.

use std::io::{self, Error, Write};

use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::composition;
use crate::rows::row_set::RowSet;
use crate::rows::slots_key::SlotsKey;
use crate::test_harness::working_app;
use crate::view;

/// A writer whose output calls fail, so CrosstermBackend fails during
/// Terminal::draw after view::draw has already populated the staged frame.
struct FailingWriter;

impl Write for FailingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(Error::other("terminal write failed"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(Error::other("terminal flush failed"))
    }
}

/// A published rect a reader can resolve against, distinct from the zeroed
/// rect a staged frame starts with.
fn published_rect() -> Rect {
    Rect::new(3, 4, 5, 6)
}

/// The staged rect the draw would write, distinct from the published one so
/// a swap is visible in the assertion.
fn drawn_rect() -> Rect {
    Rect::new(9, 9, 9, 9)
}

/// When CrosstermBackend fails to write the frame to the underlying writer,
/// the candidate frame populated by view::draw is discarded and the previous
/// published frame remains authoritative.
#[test]
fn test_failed_backend_keeps_published() {
    let options = TerminalOptions {
        viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
    };
    let mut terminal = Terminal::with_options(CrosstermBackend::new(FailingWriter), options)
        .expect("fixed-viewport terminal over a failing writer");
    let mut app = working_app();
    app.ui.frame.layout.transcript_rect.set(published_rect());

    let published = app.ui.stage_frame();
    let drawn = terminal.draw(|f| {
        view::draw(f, &app);
    });
    let settled = app.ui.settle_frame(published, drawn);

    assert!(
        settled.is_err(),
        "a failed terminal write must reach the caller"
    );
    assert_eq!(
        app.ui.frame.layout.transcript_rect.get(),
        published_rect(),
        "the last published frame comes back"
    );
}

/// The stage/publish sequence a failed draw follows, with the error supplied
/// rather than produced by a backend.
#[test]
fn test_failed_draw_keeps_published() {
    let mut app = composition::app();
    app.ui.frame.layout.transcript_rect.set(published_rect());

    let published = app.ui.stage_frame();
    assert_eq!(
        app.ui.frame.layout.transcript_rect.get(),
        Rect::default(),
        "the draw writes into a fresh frame, not the published one"
    );
    app.ui.frame.layout.transcript_rect.set(drawn_rect());

    let settled: io::Result<()> = app
        .ui
        .settle_frame(published, Err(Error::other("draw failed")));

    assert!(settled.is_err(), "the draw error still propagates");
    assert_eq!(
        app.ui.frame.layout.transcript_rect.get(),
        published_rect(),
        "the last published frame comes back"
    );
}

/// A successful draw publishes what it drew, feedback included, so a reader
/// after the draw measures the frame that was actually painted.
#[test]
fn test_successful_draw_publishes() {
    let mut app = composition::app();
    app.ui.frame.layout.transcript_rect.set(published_rect());
    app.ui.frame.feedback.transcript_width.set(40);

    let published = app.ui.stage_frame();
    app.ui.frame.layout.transcript_rect.set(drawn_rect());
    app.ui.frame.feedback.transcript_width.set(120);

    app.ui
        .settle_frame(published, Ok(()))
        .expect("the draw succeeded");

    assert_eq!(
        app.ui.frame.layout.transcript_rect.get(),
        drawn_rect(),
        "the drawn frame is the published one"
    );
    assert_eq!(
        app.ui.frame.feedback.transcript_width.get(),
        120,
        "and its measurements are the published ones"
    );
}

/// Staging a frame leaves the caches the draw reuses alone, so a new frame
/// reuses the pre-rendered slot rows instead of rebuilding them.
#[test]
fn test_stage_keeps_the_caches() {
    let mut app = composition::app();
    app.ui
        .renderer
        .slots
        .replace(SlotsKey::for_frame(&app, 80), RowSet::default());
    let _published = app.ui.stage_frame();

    assert!(
        app.ui.renderer.slots.holds(&SlotsKey::for_frame(&app, 80)),
        "the slot cache survives the frame swap"
    );
}
