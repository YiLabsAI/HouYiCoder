//! Shared test helpers: render the App to a TestBackend and dump the buffer
//! as plain text so tests can assert on what the user actually sees. Only
//! compiled under cfg(test).

#![cfg(test)]

use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

use crate::state::App;
use crate::view::draw;

/// Render the app to a TestBackend terminal of the given size and return the
/// buffer content as plain text (one line per row, trailing spaces trimmed).
pub(crate) fn render_text(app: &App, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut term = Terminal::new(backend).expect("test backend");
    term.draw(|f| {
        draw(f, app);
        crate::app::apply_selection_overlay(f, app);
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
        crate::app::apply_selection_overlay(f, app);
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
    pub lines: Vec<crate::records::TranscriptLine>,
    pub log_bytes: u64,
    pub truncated: bool,
    pub skipped: usize,
    pub window_lines: Vec<crate::records::TranscriptLine>,
    pub window_start: u64,
    pub windows: Vec<crate::transcript::snapshot::WindowLoad>,
    pub index_steps: u32,
    pub index_calls: std::sync::atomic::AtomicU32,
}

impl MockSnapshot {
    /// Find the window whose [start, next) range precedes from_byte (its
    /// next_offset == from_byte).
    fn win_before(&self, from_byte: u64) -> crate::transcript::snapshot::WindowLoad {
        self.windows
            .iter()
            .rev()
            .find(|w| w.next_offset == from_byte)
            .cloned()
            .unwrap_or_default()
    }
    fn win_at(&self, anchor: u64) -> crate::transcript::snapshot::WindowLoad {
        self.windows
            .iter()
            .find(|w| w.start_offset == anchor)
            .cloned()
            .unwrap_or_default()
    }
}

impl crate::transcript::snapshot::TranscriptSnapshot for MockSnapshot {
    fn log_size(&self) -> u64 {
        self.log_bytes
    }
    fn load(&self, _max_bytes: u64) -> crate::transcript::snapshot::SnapshotLoad {
        crate::transcript::snapshot::SnapshotLoad {
            lines: self.lines.clone(),
            skipped: self.skipped,
            truncated: self.truncated,
        }
    }
    fn tail_window(&self, _max_bytes: u64) -> crate::transcript::snapshot::WindowLoad {
        if let Some(last) = self.windows.last() {
            return last.clone();
        }
        crate::transcript::snapshot::WindowLoad {
            lines: self.window_lines.clone(),
            start_offset: self.window_start,
            next_offset: self.log_bytes,
            skipped: 0,
            bytes_total: self.log_bytes,
        }
    }
    fn window(&self, anchor: u64, _max_bytes: u64) -> crate::transcript::snapshot::WindowLoad {
        if !self.windows.is_empty() {
            return self.win_at(anchor);
        }
        crate::transcript::snapshot::WindowLoad::default()
    }
    fn window_before(
        &self,
        from_byte: u64,
        _max_bytes: u64,
    ) -> crate::transcript::snapshot::WindowLoad {
        if !self.windows.is_empty() {
            return self.win_before(from_byte);
        }
        crate::transcript::snapshot::WindowLoad::default()
    }
    fn index_chunk(&self) -> crate::transcript::snapshot::IndexProgress {
        let n = self.index_calls.load(std::sync::atomic::Ordering::Relaxed) + 1;
        self.index_calls
            .store(n, std::sync::atomic::Ordering::Relaxed);
        let done = self.index_steps > 0 && n >= self.index_steps;
        let total = self.log_bytes;
        let indexed = if done {
            total
        } else {
            (n as u64) * 4 * 1024 * 1024
        };
        crate::transcript::snapshot::IndexProgress {
            indexed_bytes: indexed.min(total),
            total_bytes: total,
            done,
        }
    }
}

/// A working-screen App for tests (no runner, stub mode).
pub(crate) fn working_app() -> App {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app
}
