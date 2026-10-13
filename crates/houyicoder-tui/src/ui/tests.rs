//! The published-frame contract: what a reader sees while a draw runs and
//! after one fails.

use std::io;

use ratatui::layout::Rect;

use crate::composition;
use crate::rows::row_set::RowSet;
use crate::rows::slots_key::SlotsKey;

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

/// A failed draw leaves the last published frame in place: the readers after
/// the draw resolve against what they last saw, not against the half-written
/// frame the failed draw was building.
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
        .settle_frame(published, Err(io::Error::other("draw failed")));

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
