//! The run loop's wake behavior: one message produces a frame promptly, a
//! burst of input produces one frame, and an empty canvas produces none.

use std::io::Write;
use std::mem::take;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use houyicoder_protocol::frontend::tasks::{
    BackgroundTaskId, BackgroundTaskSnapshot, BackgroundTaskStatus,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui_crossterm::CrosstermBackend;

use super::{BracketedWriter, FrameWriter, TerminalWriter, run};
use crate::agent_message::{ServerEvent, SessionMessage};
use crate::app::wake::UiWake;
use crate::app::{SYNC_BEGIN, SYNC_END};
use crate::input::terminal_events::EventReader;
use crate::session::{AgentSender, SessionConnection};
use crate::state::App;
use crate::termination::TerminationWatch;
use crate::test_harness::working_app;

/// A writer that records when each frame was drawn and how many background
/// tasks the app held at the moment, then tells the test a frame landed.
struct RecordingWriter {
    frames: Vec<(Instant, u64)>,
    drawn: Sender<Instant>,
}

impl RecordingWriter {
    fn new(drawn: Sender<Instant>) -> Self {
        Self {
            frames: Vec::new(),
            drawn,
        }
    }
}

impl FrameWriter for RecordingWriter {
    fn write_frame(&mut self, app: &mut App) -> std::io::Result<()> {
        let now = Instant::now();
        self.frames
            .push((now, app.sessions.current.capabilities.tasks.revision()));
        self.drawn.send(now).ok();
        Ok(())
    }
}

/// A background-task change, the smallest message whose arrival changes app
/// state a frame can show.
fn task_changed() -> SessionMessage {
    SessionMessage::Event(ServerEvent::BackgroundTaskChanged {
        task: BackgroundTaskSnapshot {
            id: BackgroundTaskId(1),
            status: BackgroundTaskStatus::Running,
            command: "cargo test".to_string(),
            elapsed_ms: 0,
            exit_code: None,
        },
    })
}

/// An app whose connection is fed only by the sender the test holds, so a
/// push lands exactly like a driver message would.
fn app_reading(agent_rx: Receiver<SessionMessage>) -> App {
    let mut app = working_app();
    app.sessions.current.connection = Some(SessionConnection::from_receiver(agent_rx));
    app
}

/// Wait for the first frame, then run the test's own action. The frame
/// receiver goes to the action so it can wait for the effect it caused
/// instead of sleeping until the effect is assumed to have landed.
fn after_first_frame(
    drawn: Receiver<Instant>,
    action: impl FnOnce(Receiver<Instant>) -> Instant + Send + 'static,
) -> thread::JoinHandle<Instant> {
    thread::spawn(move || {
        drawn
            .recv_timeout(Duration::from_secs(5))
            .expect("the first frame is drawn");
        action(drawn)
    })
}

/// A message pushed by the agent side with no keyboard input must reach a
/// frame. The loop is wake-driven, so a pushed message draws a frame without
/// waiting on a poll tick; the latch proves that happened, and a wall-clock
/// bound is not asserted because a shared CI runner varies too much for any
/// fixed budget to be stable.
#[test]
fn test_agent_message_reaches_frame() {
    let signal = UiWake::new();
    let (agent_tx, agent_rx) = channel();
    let sender = AgentSender::with_wake(agent_tx, Arc::clone(&signal));
    let (reader_tx, reader_rx) = channel();
    let reader = EventReader::over(reader_rx);
    let termination = TerminationWatch::for_test(false);
    let (drawn_tx, drawn_rx) = channel();
    let mut writer = RecordingWriter::new(drawn_tx);
    let mut app = app_reading(agent_rx);
    let wake = Arc::clone(&signal);
    let pusher = after_first_frame(drawn_rx, move |drawn| {
        let pushed = Instant::now();
        sender
            .send(task_changed())
            .expect("the loop holds the channel");
        // Latch on the frame the message caused, then end the session the way
        // a closed terminal does: the reader reports itself gone.
        drawn
            .recv_timeout(Duration::from_secs(5))
            .expect("the pushed message reaches a frame");
        drop(reader_tx);
        wake.wake();
        pushed
    });
    run(&mut app, &reader, &signal, &termination, &mut writer, None)
        .expect("the loop ends cleanly");
    let _pushed = pusher.join().expect("the pushing thread ends");
    assert!(
        writer.frames.iter().any(|(_, tasks)| *tasks > 0),
        "the pushed message reached a frame without keyboard input"
    );
}

/// A run of pointer events the reader delivers together costs one frame.
#[test]
fn test_mouse_burst_draws_once() {
    let signal = UiWake::new();
    let (reader_tx, reader_rx) = channel();
    let reader = EventReader::over(reader_rx);
    let termination = TerminationWatch::for_test(false);
    let (drawn_tx, drawn_rx) = channel();
    let mut writer = RecordingWriter::new(drawn_tx);
    let mut app = working_app();
    let wake = Arc::clone(&signal);
    let pusher = after_first_frame(drawn_rx, move |drawn| {
        for column in 0..50 {
            reader_tx
                .send(Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column,
                    row: 1,
                    modifiers: KeyModifiers::NONE,
                }))
                .expect("the loop holds the reader");
        }
        wake.wake();
        // Latch on the burst's frame, then end the session.
        drawn
            .recv_timeout(Duration::from_secs(5))
            .expect("the burst reaches a frame");
        drop(reader_tx);
        wake.wake();
        Instant::now()
    });
    run(&mut app, &reader, &signal, &termination, &mut writer, None)
        .expect("the loop ends cleanly");
    pusher.join().expect("the pushing thread ends");
    // The first frame is the loop settling; the burst is the frame after it.
    assert_eq!(
        writer.frames.len(),
        2,
        "fifty pointer events drawn as {} frames",
        writer.frames.len()
    );
}

/// A quit that lands while the loop handles a drain still ends it before the
/// frame window: a quitting loop must not wait out the cooldown or paint a
/// frame nobody asked for.
#[test]
fn test_quit_skips_draw() {
    let signal = UiWake::new();
    let (reader_tx, reader_rx) = channel();
    let reader = EventReader::over(reader_rx);
    let termination = TerminationWatch::for_test(false);
    let (drawn_tx, drawn_rx) = channel();
    let mut writer = RecordingWriter::new(drawn_tx);
    let mut app = working_app();
    let wake = Arc::clone(&signal);
    let pusher = after_first_frame(drawn_rx, move |_drawn| {
        // Two exit presses in one drain: the first raises the confirmation,
        // the second sets the quit flag, so the same turn is both dirty and
        // finished.
        for _ in 0..2 {
            reader_tx
                .send(Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char('d'),
                    KeyModifiers::CONTROL,
                    KeyEventKind::Press,
                )))
                .expect("the loop holds the reader");
        }
        wake.wake();
        drop(reader_tx);
        wake.wake();
        Instant::now()
    });
    run(&mut app, &reader, &signal, &termination, &mut writer, None)
        .expect("the loop ends cleanly");
    pusher.join().expect("the pushing thread ends");
    assert!(app.ui.quit, "the exit gesture reached the quit flag");
    assert_eq!(
        writer.frames.len(),
        1,
        "the quit left the loop before drawing a frame for the exit press"
    );
}

/// Typed text, Enter, and Esc arrive as one batch. The submit must start the
/// run before the Esc is handled, so the Esc cancels that run instead of
/// finding an idle session and doing nothing.
#[test]
fn test_submit_then_esc_aborts() {
    let signal = UiWake::new();
    let (agent_tx, agent_rx) = channel();
    let sender = AgentSender::with_wake(agent_tx, Arc::clone(&signal));
    let (reader_tx, reader_rx) = channel();
    let reader = EventReader::over(reader_rx);
    let termination = TerminationWatch::for_test(false);
    let (drawn_tx, _drawn_rx) = channel();
    let mut writer = RecordingWriter::new(drawn_tx);
    let mut app = app_reading(agent_rx);
    for code in [
        KeyCode::Char('h'),
        KeyCode::Char('i'),
        KeyCode::Enter,
        KeyCode::Esc,
    ] {
        reader_tx
            .send(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            .expect("the loop holds the reader");
    }
    // The closed channel is how the loop learns the session ended; every
    // queued event is drained before it does.
    drop(reader_tx);
    run(&mut app, &reader, &signal, &termination, &mut writer, None)
        .expect("the loop ends cleanly");
    assert!(
        app.sessions.current.run.is_cancelling(),
        "the Esc should cancel the run the Enter started"
    );
    assert_eq!(
        app.sessions.current.last_run_input.as_deref(),
        Some("hi"),
        "the submitted draft is kept so the interrupted turn can be restored"
    );
    drop(sender);
}

/// Everything the test backend holds, one line per row.
fn painted(backend: &TestBackend) -> String {
    let buffer = backend.buffer();
    let area = buffer.area;
    let mut text = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

/// A canvas with no cells publishes no frame; the same writer over a canvas
/// with cells paints the draft and publishes the composer cursor.
#[test]
fn test_empty_canvas_skips_draw() {
    let mut app = working_app();
    let mut empty = Terminal::new(TestBackend::new(0, 0)).expect("a zero-area terminal");
    TerminalWriter::new(&mut empty)
        .write_frame(&mut app)
        .expect("an empty canvas renders nothing");
    assert_eq!(
        app.ui.frame.feedback.native_cursor_position.get(),
        None,
        "no frame is published for a canvas with no cells"
    );

    app.ui.input.editor.set("hello".to_string());
    let mut full = Terminal::new(TestBackend::new(80, 24)).expect("a normal terminal");
    TerminalWriter::new(&mut full)
        .write_frame(&mut app)
        .expect("a normal canvas draws");
    assert!(
        painted(full.backend()).contains("hello"),
        "the draft reaches the canvas that has cells"
    );
    assert!(
        app.ui.frame.feedback.native_cursor_position.get().is_some(),
        "a drawn frame publishes the composer cursor"
    );
}

/// The bracket placement rides the frame stream of the backends the session
/// drives, which are writers: the begin marker orders ahead of the frame
/// bytes and the end marker behind them, on the one stream the frame rides.
/// The run loop writes the markers only for a terminal that accepted
/// synchronized output; the placement is exercised here directly because
/// the test environment's terminal reports none.
#[test]
fn test_bracket_placement_writes_stream() {
    let shared = Stream::default();
    let backend = CrosstermBackend::new(shared.clone());
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut app = working_app();
    app.ui.input.editor.set("hello".to_string());
    let mut writer = TerminalWriter::new(&mut terminal);
    writer
        .place_ahead(SYNC_BEGIN)
        .expect("the begin marker rides the stream");
    writer.write_frame(&mut app).expect("frame");
    writer
        .place_behind(SYNC_END)
        .expect("the end marker rides the stream");
    terminal.backend_mut().flush().expect("the stream flushes");
    let bytes = shared.take();
    assert!(
        bytes.starts_with(SYNC_BEGIN),
        "the begin marker orders ahead of the frame bytes: {bytes:?}"
    );
    assert!(
        bytes.ends_with(SYNC_END),
        "the end marker orders behind the frame bytes: {bytes:?}"
    );
    assert!(
        app.ui.frame.feedback.native_cursor_position.get().is_some(),
        "a drawn frame publishes the composer cursor"
    );
}

/// A byte sink the test shares with the backend, so the stream a frame
/// writes is the stream the assertion reads.
#[derive(Default, Clone)]
struct Stream(Arc<Mutex<Vec<u8>>>);

impl Stream {
    /// The bytes written so far, taken out.
    fn take(&self) -> Vec<u8> {
        take(&mut *self.0.lock().expect("stream"))
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut inner = self.0.lock().expect("stream");
        inner.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The bracketed writer forwards the frame to the writer it wraps and the
/// markers to the placement the stream owns: one wrapped frame draws, and
/// the markers a bracketed frame carries ride the same stream in order.
#[test]
fn test_bracketed_writer_forwards() {
    let shared = Stream::default();
    let backend = CrosstermBackend::new(shared.clone());
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut app = working_app();
    app.ui.input.editor.set("hello".to_string());
    {
        let mut writer = TerminalWriter::new(&mut terminal);
        let mut bracketed = BracketedWriter::new(&mut writer);
        bracketed
            .write_ahead(SYNC_BEGIN)
            .expect("the marker forwards");
        bracketed.write_frame(&mut app).expect("frame forwards");
        bracketed
            .write_behind(SYNC_END)
            .expect("the marker forwards");
    }
    assert!(
        app.ui.frame.feedback.native_cursor_position.get().is_some(),
        "the wrapped frame draws through the bracketed writer"
    );
    let bytes = shared.take();
    assert!(
        bytes.starts_with(SYNC_BEGIN) && bytes.ends_with(SYNC_END),
        "the markers ride the stream in order: {bytes:?}"
    );
}
