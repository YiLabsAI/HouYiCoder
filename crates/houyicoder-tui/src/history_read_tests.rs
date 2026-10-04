//! Lifecycle of the background history read: which events invalidate a
//! running read, and what a landed result may still touch. The fixtures
//! inject the read over a test channel and hold the sender, so the completion
//! order is controlled — no sleeps and no real worker.

use std::sync::mpsc;

use crate::composition;
use crate::records::TranscriptLine;
use crate::state::history_read::{HistoryReadOutcome, HistoryReadResult};
use crate::state::transcript::DiskFront;
use crate::state::{App, Screen};
use crate::transcript::TranscriptFrame;
use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::run::ContentBlock;
use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};

fn user_frame(text: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
        ContentBlock::Text { text: text.into() },
    )))
}

/// A working app whose resident frame log renders the given number of rows.
/// No byte drain runs (default budget), so the resident front stays at zero.
fn working_app(rows: usize) -> App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    for i in 0..rows {
        app.transcript
            .push_frame(user_frame(&format!("resident row {i}")));
    }
    app.rebuild_transcript();
    app
}

fn landed_rows(text: &str, anchor: u64) -> HistoryReadOutcome {
    HistoryReadOutcome::Rows(HistoryReadResult {
        rows: vec![TranscriptLine::User(text.into())],
        anchor,
    })
}

/// Park off the tail and inject a read stamped with the coordinates the apply
/// side re-checks, holding the sender so the test controls when it lands.
fn inject_read(app: &mut App) -> mpsc::Sender<HistoryReadOutcome> {
    app.transcript_scroll.jump_to(0);
    let (tx, rx) = mpsc::channel::<HistoryReadOutcome>();
    let front = app.transcript.frame_window_start();
    let disk_front = app.transcript.disk_front();
    app.history_reads.dispatch(front, disk_front, rx);
    tx
}

/// A clear resets the transcript while a read runs against the old history.
/// The read's rows belong to the archived session: after the reset the view
/// is a fresh history, and a landed result must not prepend into it — the
/// resident front the result was cut against no longer describes anything,
/// even when its number (zero) still matches the rebuilt view.
#[test]
fn test_reset_drops_pending_read() {
    let mut app = working_app(5);
    let tx = inject_read(&mut app);

    app.run_command(SlashCommand::Clear);

    tx.send(landed_rows("archived row", 100)).ok();
    app.pump_history_read();

    assert_eq!(app.transcript.disk_row_count(), 0);
    assert!(
        app.transcript
            .iter()
            .all(|line| line.render() != "archived row"),
        "a read dispatched before the reset never lands in the cleared view"
    );
}

/// The loop takes the record out of the slot to poll it; a clear landing in
/// that window drops the slot but cannot reach the taken record. Putting it
/// back and polling must still refuse the result: the epoch the record
/// carries no longer matches the owner's, and no rebuilt-view number can
/// fake that match.
#[test]
fn test_clear_stales_held_read() {
    let mut app = working_app(5);
    let tx = inject_read(&mut app);
    let held = app.history_reads.take().expect("a read is pending");

    app.run_command(SlashCommand::Clear);
    app.history_reads.put_back(held);

    tx.send(landed_rows("archived row", 100)).ok();
    app.pump_history_read();

    assert_eq!(
        app.transcript.disk_row_count(),
        0,
        "the epoch moved on, so the held record's result does not apply"
    );
}

/// A rewind truncates the newest turn off the tail of the frame log. The
/// pending read belongs to the older history ahead of the resident front,
/// which the rewind does not touch: none of the four apply coordinates move,
/// and the landing stays valid. Pinning this direction too — invalidation
/// must not over-fire and silently lose a read the view still wants.
#[test]
fn test_rewind_spares_older_read() {
    let mut app = working_app(5);
    let tx = inject_read(&mut app);

    app.rewind_to_last_user_input();

    tx.send(landed_rows("older row", 100)).ok();
    app.pump_history_read();

    assert_eq!(
        app.transcript.disk_row_count(),
        1,
        "a rewind of the newest turn does not stale the older-history read"
    );
}

/// A chained read continues the loaded disk-rows stack upward from where that
/// stack begins; its window is trusted without an overlap match because the
/// stack's own seam was matched when it loaded. Returning to the tail releases
/// the stack, so the basis of that trust is gone: the stale result must not
/// apply. Applying it would prepend rows cut against a seam the view no
/// longer holds — the fresh scroll-back has to re-dispatch a tail read that
/// re-matches the overlap against the current front row.
#[test]
fn test_tail_return_stales_chain() {
    let mut app = working_app(30);
    let front = app.transcript.frame_window_start();
    app.transcript.prepend_disk_rows(
        vec![TranscriptLine::User("loaded older".into())],
        4096,
        front,
    );
    assert_eq!(app.transcript.disk_front(), DiskFront::At(4096));
    let tx = inject_read(&mut app);

    // The reader returns to the tail: the loaded stack is released.
    app.transcript_scroll.follow_tail();
    app.rebuild_transcript();
    assert_eq!(
        app.transcript.disk_front(),
        DiskFront::Unloaded,
        "the tail return released the loaded stack"
    );

    // The reader scrolls back up while the stale chain read still holds the
    // slot, and the result lands.
    app.transcript_scroll.jump_to(0);
    tx.send(landed_rows("chained older", 100)).ok();
    app.pump_history_read();

    assert_eq!(
        app.transcript.disk_row_count(),
        0,
        "a chain read whose loaded stack was released does not apply"
    );
}
