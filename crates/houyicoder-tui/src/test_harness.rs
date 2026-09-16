//! TUI fixtures for rendered output, session connections, and snapshots.

#![cfg(test)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use houyicoder_async::PFut;
use houyicoder_client::{Client, Transport};
use houyicoder_protocol::envelope::{ClientFrame, RequestEnvelope};
use houyicoder_protocol::error::{ErrorCategory, ProtocolError};
use houyicoder_protocol::frontend::FrontendRequest;
use houyicoder_protocol::handshake::Hello;
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

use crate::agent_message::{ConnectionEvent, SessionMessage};
use crate::app::apply_selection_overlay;
use crate::records::TranscriptLine;
use crate::session::SessionConnection;
use crate::state::{App, Screen};
use crate::transcript::snapshot::{IndexProgress, SnapshotLoad, TranscriptSnapshot, WindowLoad};
use crate::view::draw;

/// Render the app to a TestBackend terminal of the given size and return the
/// buffer content as plain text (one line per row, trailing spaces trimmed).
pub(crate) fn render_text(app: &App, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).expect("test backend");
    term.draw(|f| {
        draw(f, app);
        apply_selection_overlay(f, app);
    })
    .expect("draw");
    dump_buffer(term.backend().buffer())
}

/// Render the app and return the raw buffer so tests can assert on cell STYLE
/// (fg color, modifiers) — not just the text. The text-dump helper above strips
/// style, so color/animation (the border shimmer, the spinner glimmer) can only
/// be verified at the cell level, which is the kind of real-interaction check
/// the text-assertion tests missed.
pub(crate) fn render_buffer(app: &App, w: u16, h: u16) -> Buffer {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).expect("test backend");
    term.draw(|f| {
        draw(f, app);
        apply_selection_overlay(f, app);
    })
    .expect("draw");
    term.backend().buffer().clone()
}

/// Read out a ratatui buffer as plain text rows.
pub(crate) fn dump_buffer(buf: &Buffer) -> String {
    let area = buf.area();
    let mut rows: Vec<String> = Vec::with_capacity(area.height as usize);
    for y in 0..area.height {
        let mut row = String::with_capacity(area.width as usize);
        for x in 0..area.width {
            let cell = buf.cell((x, y)).expect("cell");
            row.push_str(cell.symbol());
        }
        rows.push(row.trim_end().to_string());
    }
    rows.join("\n")
}

/// A test-only TranscriptSnapshot returning a prebuilt load. Lets tests
/// exercise the search-view load path (enter calls load) without a real
/// session log + backend. Also supports the byte-window path: when log_bytes
/// is over the threshold, enter_search_view calls tail_window instead of
/// load. For single-window tests, window_lines + window_start build that one
/// window; for multi-window scan tests, windows (oldest first, each with its
/// byte range) drives tail_window/window/window_before. index_steps simulates
/// a multi-chunk full scan (0 = never done, for the Esc-interrupt test).
pub(crate) struct MockSnapshot {
    pub lines: Vec<TranscriptLine>,
    pub log_bytes: u64,
    pub truncated: bool,
    pub skipped: usize,
    pub window_lines: Vec<TranscriptLine>,
    pub window_start: u64,
    pub windows: Vec<WindowLoad>,
    pub index_steps: u32,
    pub index_calls: AtomicU32,
}

impl MockSnapshot {
    /// Find the window whose [start, next) range precedes from_byte (its
    /// next_offset == from_byte).
    fn win_before(&self, from_byte: u64) -> WindowLoad {
        self.windows
            .iter()
            .rev()
            .find(|w| w.next_offset == from_byte)
            .cloned()
            .unwrap_or_default()
    }
    fn win_at(&self, anchor: u64) -> WindowLoad {
        self.windows
            .iter()
            .find(|w| w.start_offset == anchor)
            .cloned()
            .unwrap_or_default()
    }
}

impl TranscriptSnapshot for MockSnapshot {
    fn log_size(&self) -> u64 {
        self.log_bytes
    }
    fn load(&self, _max_bytes: u64) -> SnapshotLoad {
        SnapshotLoad {
            lines: self.lines.clone(),
            skipped: self.skipped,
            truncated: self.truncated,
        }
    }
    fn tail_window(&self, _max_bytes: u64) -> WindowLoad {
        if let Some(last) = self.windows.last() {
            return last.clone();
        }
        WindowLoad {
            lines: self.window_lines.clone(),
            start_offset: self.window_start,
            next_offset: self.log_bytes,
            skipped: 0,
            bytes_total: self.log_bytes,
        }
    }
    fn window(&self, anchor: u64, _max_bytes: u64) -> WindowLoad {
        if !self.windows.is_empty() {
            return self.win_at(anchor);
        }
        WindowLoad::default()
    }
    fn window_before(&self, from_byte: u64, _max_bytes: u64) -> WindowLoad {
        if !self.windows.is_empty() {
            return self.win_before(from_byte);
        }
        WindowLoad::default()
    }
    fn index_chunk(&self) -> IndexProgress {
        let n = self.index_calls.load(Ordering::Relaxed) + 1;
        self.index_calls.store(n, Ordering::Relaxed);
        let done = self.index_steps > 0 && n >= self.index_steps;
        let total = self.log_bytes;
        let indexed = if done {
            total
        } else {
            (n as u64) * 4 * 1024 * 1024
        };
        IndexProgress {
            indexed_bytes: indexed.min(total),
            total_bytes: total,
            done,
        }
    }
}

/// A working-screen App for tests (no runner, disconnected).
pub(crate) fn working_app() -> App {
    let mut app = crate::composition::app();
    app.screen = Screen::Working;
    app
}

pub(crate) enum TransportEvent {
    Frame(String),
    Dropped,
}

/// A negotiated transport probe that reports frame and shutdown events.
struct RecordingTransport {
    hello_sent: bool,
    events: Sender<TransportEvent>,
}

impl RecordingTransport {
    fn new(events: Sender<TransportEvent>) -> Self {
        Self {
            hello_sent: false,
            events,
        }
    }
}

impl Drop for RecordingTransport {
    fn drop(&mut self) {
        drop(self.events.send(TransportEvent::Dropped));
    }
}

impl Transport for RecordingTransport {
    fn send_frame(&mut self, frame: &str) -> PFut<'_, Result<(), ProtocolError>> {
        drop(self.events.send(TransportEvent::Frame(frame.to_string())));
        Box::pin(async { Ok(()) })
    }

    fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, ProtocolError>> {
        if !self.hello_sent {
            self.hello_sent = true;
            let hello = serde_json::to_string(&Hello::local()).expect("hello serializes");
            Box::pin(async move { Ok(Some(hello)) })
        } else {
            Box::pin(std::future::pending())
        }
    }
}

/// Build an App with an active negotiated test session.
pub(crate) fn connected_app() -> App {
    connected_app_with_events().0
}

/// Build a connected App plus the transport's outbound frame log: each
/// frame the session ships arrives as a TransportEvent, so tests can pin
/// exactly what a command path put on the connection.
pub(crate) fn connected_app_events() -> (App, Receiver<TransportEvent>) {
    connected_app_with_events()
}

fn connected_app_with_events() -> (App, Receiver<TransportEvent>) {
    let runtime = crate::composition::shared_runtime();
    let (events_tx, events_rx) = mpsc::channel();
    let client = Client::new(Box::new(RecordingTransport::new(events_tx)));
    let (agent_tx, agent_rx) = mpsc::channel::<SessionMessage>();
    let session = SessionConnection::spawn(client, agent_tx, agent_rx, &runtime);
    let mut app = working_app();
    app.runtime = Some(runtime);
    app.session = Some(session);
    (app, events_rx)
}

/// Wait for the connected transport to ship a request frame whose payload
/// matches the predicate, then return its envelope (req_id + payload). Frame
/// arrival is the synchronization point: recv_timeout blocks until the next transport
/// event and skips non-matching frames, so no polling sleep is needed. Panics
/// when the transport drops or no matching request arrives within two seconds.
pub(crate) fn wait_for_request(
    events: &Receiver<TransportEvent>,
    want: impl Fn(&FrontendRequest) -> bool,
) -> RequestEnvelope {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(remaining) {
            Ok(TransportEvent::Frame(frame)) => {
                if let Ok(ClientFrame::Request(env)) = serde_json::from_str::<ClientFrame>(&frame)
                    && want(&env.payload)
                {
                    return env;
                }
            }
            Ok(TransportEvent::Dropped) => panic!("transport dropped before the matching request"),
            Err(_) => panic!("no matching request frame within 2s"),
        }
    }
}

/// A transport whose handshake fails immediately: the driver exits and drops
/// the command receiver, so every later send is refused. This is the
/// connection-lost state — a session object that exists, a driver that does
/// not.
pub(crate) struct FailedHandshakeTransport;

impl Transport for FailedHandshakeTransport {
    fn send_frame(&mut self, _frame: &str) -> PFut<'_, Result<(), ProtocolError>> {
        Box::pin(async { Ok(()) })
    }
    fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, ProtocolError>> {
        Box::pin(async {
            Err(ProtocolError::new(
                ErrorCategory::Unavailable,
                "no server",
                false,
            ))
        })
    }
}

/// Attach a live recording connection to an existing test App, so send
/// paths succeed without rebuilding the App. Use this when a fixture built
/// its own state (catalog, queues, cards) must now take its connected
/// branch.
pub(crate) fn attach_connection(app: &mut App) {
    let runtime = crate::composition::shared_runtime();
    let (events_tx, _events_rx) = mpsc::channel();
    let client = Client::new(Box::new(RecordingTransport::new(events_tx)));
    let (agent_tx, agent_rx) = mpsc::channel::<SessionMessage>();
    let session = SessionConnection::spawn(client, agent_tx, agent_rx, &runtime);
    app.runtime = Some(runtime);
    app.session = Some(session);
}

/// An App whose driver exited on a failed handshake: the session object is
/// present, the connection is lost, and the Lost event has already
/// arrived. Send attempts are refused deterministically, so tests can drive
/// the send-failure branches without racing the scheduler.
pub(crate) fn connection_lost_app() -> App {
    let runtime = crate::composition::shared_runtime();
    let client = Client::new(Box::new(FailedHandshakeTransport));
    let (agent_tx, agent_rx) = mpsc::channel::<SessionMessage>();
    let mut session = SessionConnection::spawn(client, agent_tx, agent_rx, &runtime);
    // Synchronization point: the driver announces its own death; from this
    // point the enqueue path is deterministically refused.
    let death = session
        .poll_startup(Duration::from_secs(5))
        .expect("the failed handshake reports the Lost event");
    let mut app = working_app();
    app.runtime = Some(runtime);
    app.session = Some(session);
    // Apply the death so the app holds the settled state: Lost with its
    // cause, active run swept. Tests start from the post-loss world instead
    // of half-applying the event themselves.
    let SessionMessage::Connection(ConnectionEvent::Lost { cause }) = death else {
        panic!("expected ConnectionEvent::Lost, got {death:?}");
    };
    app.handle_agent_message(SessionMessage::Connection(ConnectionEvent::Lost { cause }));
    app
}

#[test]
fn test_harness_sends_frames() {
    let (mut app, events) = connected_app_with_events();
    app.tab_cycle_mode();
    loop {
        match events
            .recv_timeout(Duration::from_secs(1))
            .expect("transport event")
        {
            TransportEvent::Frame(frame) if frame.contains("PermissionCycleMode") => break,
            TransportEvent::Frame(_) => {}
            TransportEvent::Dropped => panic!("transport dropped before the request"),
        }
    }
}

#[test]
fn test_harness_stops_driver() {
    let (app, events) = connected_app_with_events();
    drop(app);
    loop {
        match events
            .recv_timeout(Duration::from_secs(1))
            .expect("transport event")
        {
            TransportEvent::Dropped => break,
            TransportEvent::Frame(_) => {}
        }
    }
}
