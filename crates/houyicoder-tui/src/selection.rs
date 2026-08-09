//! In-app text selection + clipboard write. While mouse capture is on the
//! terminal native selection is unavailable, so the app owns selection and
//! mouse-up writes the dragged text to the clipboard (native tool + OSC 52
//! fallback) so paste just works.

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Instant;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ratatui::layout::Rect;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Multi-click window: a second press within this time and within this many
/// cells of the previous press counts as a double/triple click.
const MULTI_CLICK_TIMEOUT_MS: u128 = 500;
const MULTI_CLICK_DISTANCE: u16 = 1;

/// Per-row style/kind tag stashed alongside the rendered text so copy and the
/// overlay can skip non-content rows (the spinner) — a cheaper stand-in for
/// a per-cell no-select bitmap, since the engine's only inline chrome is
/// the transient spinner row at the tail.
pub const TAG_PLAIN: u8 = 0;
pub const TAG_USER: u8 = 1;
pub const TAG_SYSTEM: u8 = 2;
pub const TAG_SPINNER: u8 = 3;
/// Diff-body line tags (a colored Edit/MultiEdit result continuation row).
/// Additions green, removals red, hunk headers cyan; context lines reuse
/// TAG_PLAIN (dim). All are content rows — copy/selection does not skip them.
pub const TAG_DIFF_ADD: u8 = 4;
pub const TAG_DIFF_DEL: u8 = 5;
pub const TAG_DIFF_HUNK: u8 = 6;
/// The first line of an agent reply carries the leading glyph prefix
/// (a circle plus a space, 2 display cols). It is non-content chrome,
/// so copy skips the first 2 cols of these rows and the glyph is
/// excluded from the clipboard — a no-select bitmap over the glyph cols.
/// The glyph is still rendered on screen, just not copied.
pub const TAG_AGENT_FIRST: u8 = 7;
/// A collapsed-fold-group summary row or an expanded-group collapse hint.
/// Not real content — copy/selection skips it, and a click toggles the fold
/// group instead of starting a selection.
pub const TAG_FOLD: u8 = 8;
/// Rows painted by an inline widget (the /context usage block): the row
/// string carries no text, the pixels come from a direct widget draw. Not
/// selectable and never copied — word-selecting one used to grab a full
/// line of nothing and clobber the clipboard with whitespace.
pub const TAG_WIDGET: u8 = 9;
/// A diff context (unchanged) line in a structured-diff body. Distinct from
/// TAG_PLAIN so the copy path can strip the line-number gutter from diff
/// context rows the same way it does add/remove rows (the gutter is wrapped
/// in a no-select span so fullscreen copy yields clean code).
pub const TAG_DIFF_CTX: u8 = 10;
/// A top or bottom dashed border row framing a structured-diff block (a
/// dashed frame: dashed top + bottom, no left/right). Non-content
/// chrome — copy/selection skips it, and diff_row paints it as a full-width
/// dim dashed line.
pub const TAG_DIFF_BORDER: u8 = 11;

/// True when the row tag marks a non-content row (spinner, fold summary,
/// widget-painted, the inter-hunk "..." gap, or a diff border):
/// excluded from selection, overlay paint, and copy.
pub fn is_non_selectable(tag: u8) -> bool {
    tag == TAG_SPINNER
        || tag == TAG_FOLD
        || tag == TAG_WIDGET
        || tag == TAG_DIFF_HUNK
        || tag == TAG_DIFF_BORDER
}

/// The number of leading display columns to skip when copying a structured-
/// diff content row (the line-number + sigil gutter, wrapped in a
/// no-select span so a fullscreen drag-copy yields clean code, not line
/// numbers). The gutter is a rigid prefix: a 4-space indent, the right-
/// aligned line number (spaces + digits), a separator space, the sigil,
/// and a separator space. Returns 0 for a row that does not match this
/// shape (a non-diff plain row), so non-diff rows copy verbatim.
pub fn diff_gutter_skip(row: &str) -> usize {
    let bytes = row.as_bytes();
    // The marker (sigil) at col 0: + / - / space. No sigil ⇒ not a diff
    // content row — copy verbatim so a plain non-diff row is not mangled.
    if !bytes
        .first()
        .is_some_and(|b| matches!(b, b'+' | b'-' | b' '))
    {
        return 0;
    }
    let mut i = 1;
    // Separator space after the marker.
    if i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    } else {
        return 0;
    }
    // Right-align leading spaces before the number.
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    let num_start = i;
    // The line-number digits.
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    // No digits ⇒ a soft-wrapped continuation row (blank gutter) — copy
    // verbatim for now (the leading blank gutter is preserved as spaces).
    if i == num_start {
        return 0;
    }
    // Trailing separator space before the content.
    if i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    i
}

/// The snap unit of a multi-click span: word (double-click) or whole line
/// (triple-click). A drag from the span extends by this unit — the
/// anchor-span kind that drives how a multi-click drag grows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpanKind {
    Word,
    Line,
}

/// The initial word/line bounds from a multi-click. A subsequent drag
/// extends FROM this span to the word/line at the cursor, so the original
/// word/line stays selected even when dragging backward past it.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub lo: (u16, usize),
    pub hi: (u16, usize),
    pub kind: SpanKind,
}

/// An in-app selection range with CONTENT SPACE as the single source of
/// truth. Both endpoints are (column, content-row) where the content row
/// indexes the FULL transcript row set, drift-free across scroll. Screen
/// positions are never stored: mouse coordinates convert to content space
/// at event time (via the scroll offset the user was looking at), and the
/// overlay maps content rows back to screen rows at paint time via the
/// then-current offset. Keeping a single coordinate space removes the
/// whole class of screen/content sync-drift bugs the previous dual
/// bookkeeping produced.
#[derive(Default, Clone, Debug)]
pub struct Selection {
    /// Where the drag started: (column, content row).
    pub anchor: Option<(u16, usize)>,
    /// Current drag position: (column, content row).
    pub focus: Option<(u16, usize)>,
    /// The last mouse position in SCREEN space, cached as an input for the
    /// poll-tick edge auto-scroll (the mouse is stationary at the edge, so
    /// no new event arrives; the tick re-derives the focus content row from
    /// this screen position plus the post-scroll offset). Never used for
    /// painting or extraction.
    pub cursor: Option<(u16, u16)>,
    pub is_dragging: bool,
    /// Last press (time, x, y, count) for multi-click detection. Count
    /// advances within the timeout+distance window; a fresh press resets to 1.
    pub last_click: Option<(Instant, u16, u16, u8)>,
    /// When a word/line select sets the initial span, a subsequent drag
    /// extends word- or line-wise from this origin (None = char-mode drag).
    pub span_origin: Option<Span>,
    /// True once the pointer actually moved during the current gesture.
    /// On release a moved gesture resets the multi-click chain, so the next
    /// press starts a fresh char-mode selection instead of escalating to
    /// word/line select (repeated drag attempts at the same spot otherwise
    /// count as double/triple clicks). Stationary presses keep the chain so
    /// double/triple click still works on jittery trackpads.
    pub drag_moved: bool,
}

impl Selection {
    /// True when a non-empty range exists (anchor + focus both set).
    pub fn has_selection(&self) -> bool {
        self.anchor.is_some() && self.focus.is_some()
    }

    /// Begin a char-mode drag at (column, content row). Drops any span
    /// origin left by a previous word/line select — without this, the next
    /// drag walked the extend-span path and teleported the anchor to the
    /// stale span rows, selecting an unrelated block.
    pub fn start(&mut self, col: u16, content_row: usize) {
        self.anchor = Some((col, content_row));
        // Focus stays unset until the first REAL drag motion (the
        // start-selection rule): a bare click-release never highlights a cell,
        // and a trackpad tremor or phantom Down+Drag pair during two-
        // finger scrolling never creates a 1-cell selection that would
        // clobber the clipboard.
        self.focus = None;
        self.span_origin = None;
        self.is_dragging = true;
        self.drag_moved = false;
    }

    /// Promote a char-mode drag whose ANCHOR sits in the blank tail of its
    /// row (beyond the text end) into a word-mode span over the whole blank
    /// run — a press in the blank area plus
    /// any nudge selects the entire blank stretch in one gesture instead of
    /// growing cell by cell. No-op when a span already exists, the anchor
    /// is inside text, or the row is not selectable.
    pub fn promote_blank_anchor(&mut self, all_rows: &[(u8, String)], rect: Rect) {
        if self.span_origin.is_some() {
            return;
        }
        let Some((col, content_row)) = self.anchor else {
            return;
        };
        let Some((tag, text)) = all_rows.get(content_row) else {
            return;
        };
        if is_non_selectable(*tag) {
            return;
        }
        let c = (col.saturating_sub(rect.x)) as usize;
        if c < UnicodeWidthStr::width(text.as_str()) {
            return;
        }
        let (lo, hi) = word_bounds_at(text, c, rect.width as usize);
        self.span_origin = Some(Span {
            lo: (rect.x + lo as u16, content_row),
            hi: (rect.x + hi.saturating_sub(1) as u16, content_row),
            kind: SpanKind::Word,
        });
    }

    /// Extend the drag focus to (column, content row). The first motion at
    /// the anchor cell is a no-op (the update-selection rule): terminals in
    /// drag-tracking mode can fire a drag event at the press cell from sub-
    /// pixel tremor, which must not turn a bare click into a selection.
    /// Once a real motion sets the focus, tracking continues normally,
    /// including back onto the anchor cell.
    pub fn update(&mut self, col: u16, content_row: usize) {
        if !self.is_dragging {
            return;
        }
        if self.focus.is_none() && self.anchor == Some((col, content_row)) {
            return;
        }
        self.drag_moved = true;
        self.focus = Some((col, content_row));
    }

    /// End the drag (keep the range so the highlight persists until cleared).
    pub fn finish(&mut self) {
        self.is_dragging = false;
    }

    /// Drop the range entirely.
    pub fn clear(&mut self) {
        self.anchor = None;
        self.focus = None;
        self.cursor = None;
        self.is_dragging = false;
        self.span_origin = None;
        self.drag_moved = false;
    }

    /// Record a press at (x, y) and return the click count (1, 2, or 3). A
    /// press within MULTI_CLICK_TIMEOUT_MS and MULTI_CLICK_DISTANCE of the
    /// previous press advances the count (capped at 3); otherwise it resets
    /// to 1. The Up handler drops the chain after a moved drag (see
    /// drag_moved) so repeated drag attempts never escalate to word/line
    /// select; stationary presses keep the chain for double/triple click.
    pub fn on_click(&mut self, x: u16, y: u16) -> u8 {
        let now = Instant::now();
        let count = match self.last_click {
            Some((t, lx, ly, c))
                if now.duration_since(t).as_millis() <= MULTI_CLICK_TIMEOUT_MS
                    && x.abs_diff(lx) <= MULTI_CLICK_DISTANCE
                    && y.abs_diff(ly) <= MULTI_CLICK_DISTANCE =>
            {
                (c + 1).min(3)
            }
            _ => 1,
        };
        self.last_click = Some((now, x, y, count));
        count
    }

    /// Select the word at (column, content row) by scanning same-class
    /// graphemes left/right in the full row set. Sets anchor/focus to the
    /// word bounds and records the span origin so a following drag extends
    /// word-wise. Whitespace click → single cell.
    pub fn select_word(
        &mut self,
        all_rows: &[(u8, String)],
        rect: Rect,
        col: u16,
        content_row: usize,
    ) {
        let Some((tag, row)) = all_rows.get(content_row) else {
            return;
        };
        if is_non_selectable(*tag) {
            return;
        }
        let (lo, hi) = word_bounds_at(
            row,
            (col.saturating_sub(rect.x)) as usize,
            rect.width as usize,
        );
        let start = rect.x + lo as u16;
        let end = rect.x + hi.saturating_sub(1) as u16;
        self.anchor = Some((start, content_row));
        self.focus = Some((end, content_row));
        self.span_origin = Some(Span {
            lo: (start, content_row),
            hi: (end, content_row),
            kind: SpanKind::Word,
        });
        self.is_dragging = true;
        self.drag_moved = false;
    }

    /// Select the whole line at the content row (rect left to rect right).
    pub fn select_line(&mut self, rect: Rect, content_row: usize) {
        let right = rect.x + rect.width.saturating_sub(1);
        self.anchor = Some((rect.x, content_row));
        self.focus = Some((right, content_row));
        self.span_origin = Some(Span {
            lo: (rect.x, content_row),
            hi: (right, content_row),
            kind: SpanKind::Line,
        });
        self.is_dragging = true;
        self.drag_moved = false;
    }

    /// Extend a word/line-mode selection to the word/line at (column,
    /// content row). The span origin (the multi-clicked word/line) stays
    /// fully selected; the selection grows from that span to the unit under
    /// the cursor, with the anchor swapping to the FAR span edge when the
    /// drag goes backward — the anchor swaps to the FAR span edge so the
    /// original word/line is never partially deselected. No-op if no span
    /// origin (char-mode drag).
    pub fn extend_span(
        &mut self,
        all_rows: &[(u8, String)],
        rect: Rect,
        col: u16,
        content_row: usize,
    ) {
        let Some(span) = self.span_origin else {
            return;
        };
        // The word/line unit under the cursor, as (lo, hi) points. Word
        // mode falls back to the raw cell when no row text is available so
        // dragging over blank rows still extends.
        let (m_lo, m_hi) = match span.kind {
            SpanKind::Word => match all_rows.get(content_row) {
                Some((_, row)) => {
                    let (lo, hi) = word_bounds_at(
                        row,
                        (col.saturating_sub(rect.x)) as usize,
                        rect.width as usize,
                    );
                    (
                        (rect.x + lo as u16, content_row),
                        (rect.x + hi.saturating_sub(1) as u16, content_row),
                    )
                }
                None => ((col, content_row), (col, content_row)),
            },
            SpanKind::Line => (
                (rect.x, content_row),
                (rect.x + rect.width.saturating_sub(1), content_row),
            ),
        };
        // Reading order compare: row first, then column.
        let before = |a: (u16, usize), b: (u16, usize)| (a.1, a.0) < (b.1, b.0);
        let (new_anchor, new_focus) = if before(m_hi, span.lo) {
            // Cursor unit ends before the span: extend backward.
            (span.hi, m_lo)
        } else if before(span.hi, m_lo) {
            // Cursor unit starts after the span: extend forward.
            (span.lo, m_hi)
        } else {
            // Cursor overlaps the span: just the span.
            (span.lo, span.hi)
        };
        if self.focus != Some(new_focus) {
            self.drag_moved = true;
        }
        self.anchor = Some(new_anchor);
        self.focus = Some(new_focus);
    }

    /// Reading-order-normalized bounds: ((start_col, start_row), (end_col,
    /// end_row)) ordered by content row then column. None if no range.
    /// Inclusive on both ends; each endpoint keeps its own column.
    pub fn bounds(&self) -> Option<((u16, usize), (u16, usize))> {
        let (a, f) = match (self.anchor, self.focus) {
            (Some(a), Some(f)) => (a, f),
            _ => return None,
        };
        if (a.1, a.0) <= (f.1, f.0) {
            Some((a, f))
        } else {
            Some((f, a))
        }
    }

    /// True if the anchor and focus sit on the same cell (a click with no
    /// drag). Used to skip copy on a plain click so the clipboard is not
    /// clobbered by a single character.
    pub fn is_click_only(&self) -> bool {
        match (self.anchor, self.focus) {
            (Some(a), Some(f)) => a == f,
            _ => true,
        }
    }

    /// True if (column, content row) falls inside the selection range in
    /// reading order: strictly between the endpoint rows, or on an endpoint
    /// row within its column bound.
    pub fn contains(&self, col: u16, content_row: usize) -> bool {
        let Some(((sx, sr), (ex, er))) = self.bounds() else {
            return false;
        };
        if content_row < sr || content_row > er {
            return false;
        }
        if sr == er {
            return col >= sx.min(ex) && col <= sx.max(ex);
        }
        if content_row == sr {
            return col >= sx;
        }
        if content_row == er {
            return col <= ex;
        }
        true
    }
}

/// Write text to the clipboard. On macOS use pbcopy (reliable in terminals
/// that disable OSC 52 by default); elsewhere emit OSC 52 via crossterm so
/// SSH / remote sessions still work. Fire-and-forget; failures are ignored.
/// Pluggable clipboard sink so adversarial selection tests can capture what a
/// drag-to-copy would write, without touching the real OS clipboard or
/// emitting OSC 52. Production holds a SystemClipboard; tests inject a
/// RecordingClipboard and assert the exact copied text.
pub trait ClipboardWriter: Send + Sync {
    fn write(&self, text: &str);
}

/// The production clipboard writer: pbcopy on macOS (reliable even when the
/// terminal disables OSC 52), OSC 52 elsewhere for SSH/remote sessions.
pub struct SystemClipboard;
impl ClipboardWriter for SystemClipboard {
    fn write(&self, text: &str) {
        write_clipboard(text);
    }
}

#[cfg(test)]
/// Test-only clipboard that records every write so adversarial selection
/// tests can assert the exact copied text without touching the OS clipboard.
pub struct RecordingClipboard {
    pub captured: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}
#[cfg(test)]
impl ClipboardWriter for RecordingClipboard {
    fn write(&self, text: &str) {
        self.captured
            .lock()
            .expect("recording clipboard")
            .push(text.to_string());
    }
}

/// Write text to the clipboard via the most robust path available. The
/// strategy: (1) a native safety net fired FIRST in a
/// detached thread (pbcopy / wl-copy / xclip / xsel / clip.exe), gated on
/// SSH_CONNECTION so it never writes to a remote clipboard over SSH; (2)
/// inside tmux, load-buffer -w propagates to the outer terminal, and a
/// DCS-passthrough-wrapped OSC 52 is also emitted; (3) raw OSC 52 (BEL
/// terminator) as the final fallback. Fire-and-forget; failures are silent.
pub fn write_clipboard(text: &str) {
    let b64 = STANDARD.encode(text.as_bytes());
    // Native net fires first in a thread so OSC 52 is not delayed by a
    // subprocess. Gated on SSH_CONNECTION: over SSH these hit the remote
    // clipboard, not the local one.
    if std::env::var_os("SSH_CONNECTION").is_none() {
        let owned = text.to_string();
        std::thread::spawn(move || copy_native(&owned));
    }
    // tmux fast path: load-buffer reaches the outer terminal via the
    // multiplexer own clipboard bridge (-w, dropped for iTerm2). On
    // success also emit a passthrough-wrapped OSC 52.
    if std::env::var_os("TMUX").is_some() && tmux_load_buffer(text) {
        let _r = emit_raw(&tmux_passthrough_osc52(&b64));
        return;
    }
    let _r = emit_raw(&raw_osc52(&b64));
}

/// Which path write_clipboard will take, based on env state. Sync so a
/// caller can show an honest toast without running the copy. Native is only
/// claimed on macOS without an SSH session; the Linux probe is lazy.
pub fn get_clipboard_path() -> &'static str {
    clipboard_path_for(
        cfg!(target_os = "macos") && std::env::var_os("SSH_CONNECTION").is_none(),
        std::env::var_os("TMUX").is_some(),
    )
}

/// Pure decision used by get_clipboard_path so tests exercise the
/// SSH-gating and tmux fallback logic without mutating process env.
pub(crate) fn clipboard_path_for(native_available: bool, in_tmux: bool) -> &'static str {
    if native_available {
        "native"
    } else if in_tmux {
        "tmux-buffer"
    } else {
        "osc52"
    }
}

/// Raw OSC 52: ESC ] 52 ; c ; <base64> BEL. BEL is more widely parsed than
/// ST (ESC backslash) for OSC 52 across terminals.
pub(crate) fn raw_osc52(b64: &str) -> String {
    format!("\x1b]52;c;{b64}\x07")
}

/// Wrap an OSC 52 payload in tmux DCS passthrough (ESC P tmux ; <payload
/// with inner ESCs doubled> ESC backslash). tmux forwards it to the outer
/// terminal, bypassing its own parser. Needs allow-passthrough on;
/// otherwise the DCS is silently dropped (no worse than the raw fallback).
pub(crate) fn tmux_passthrough_osc52(b64: &str) -> String {
    let escaped = raw_osc52(b64).replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{escaped}\x1b\\")
}

/// Load text into the tmux paste buffer via load-buffer. -w (tmux 3.2+)
/// propagates to the outer terminal via the multiplexer own OSC 52; dropped
/// for iTerm2 where that emission crashes an SSH session.
// The clipboard helpers are a pure-client local copy path (tmux load-buffer,
// pbcopy); they live above the layer that owns the spawn chokepoint and must
// not pull a ports dependency, so the chokepoint rule is allowed here.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
fn tmux_load_buffer(text: &str) -> bool {
    let args: &[&str] = if std::env::var_os("LC_TERMINAL").is_some_and(|v| v == "iTerm2") {
        &["load-buffer", "-"]
    } else {
        &["load-buffer", "-w", "-"]
    };
    match Command::new("tmux")
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _r = stdin.write_all(text.as_bytes());
                drop(stdin);
            }
            child.wait().map(|s| s.success()).unwrap_or(false)
        }
        Err(_) => false,
    }
}

/// Shell out to a native clipboard utility as a safety net for OSC 52. Only
/// called when not in an SSH session; failures are silent. The Linux winner
/// is cached after the first probe so repeated mouse-ups skip the chain.
fn copy_native(text: &str) {
    #[cfg(target_os = "macos")]
    {
        let _r = run_native("pbcopy", &[], text);
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(tool) = linux_clipboard_tool(text) {
            let args: &[&str] = match tool {
                "wl-copy" => &[],
                "xclip" => &["-selection", "clipboard"],
                "xsel" => &["--clipboard", "--input"],
                _ => &[],
            };
            let _r = run_native(tool, args, text);
        }
    }
    #[cfg(target_os = "windows")]
    {
        let _r = run_native("clip", &[], text);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = text;
    }
}

/// Run a native clipboard binary, feeding text via stdin. Ok on a clean
/// (exit-0) run so the Linux probe can pick a winner.
#[expect(clippy::disallowed_methods, reason = "infra spawn, not model-driven")]
fn run_native(bin: &str, args: &[&str], text: &str) -> std::io::Result<()> {
    let mut child = Command::new(bin).args(args).stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
        drop(stdin);
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("non-zero exit"))
    }
}

/// Cached Linux clipboard tool. The probe runs on the FIRST copy with the
/// actual text (xclip / xsel would hang on an empty probe stdin), then the
/// winner is cached so later copies skip the chain. wl-copy only when
/// WAYLAND_DISPLAY is set; xclip then xsel cover X11.
static LINUX_TOOL: Mutex<Option<Option<&'static str>>> = Mutex::new(None);

fn linux_clipboard_tool(text: &str) -> Option<&'static str> {
    {
        let cache = LINUX_TOOL.lock().expect("linux tool cache");
        match *cache {
            Some(Some(tool)) => return Some(tool),
            Some(None) => return None,
            None => {}
        }
    }
    let winner = if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && run_native("wl-copy", &[], text).is_ok()
    {
        Some("wl-copy")
    } else if run_native("xclip", &["-selection", "clipboard"], text).is_ok() {
        Some("xclip")
    } else if run_native("xsel", &["--clipboard", "--input"], text).is_ok() {
        Some("xsel")
    } else {
        None
    };
    *LINUX_TOOL.lock().expect("linux tool cache") = Some(winner);
    winner
}

/// Write a byte sequence to stdout (locked, flushed). OSC 52 is a
/// fire-and-forget emit; a write failure is silent.
fn emit_raw(seq: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(seq.as_bytes())?;
    out.flush()
}

/// Extract the selected text from a transcript row set indexed by content
/// row. Cells are addressed by display column (CJK width 2), so the
/// substring is sliced by accumulated grapheme width, not byte offset. The
/// un-normalized anchor/focus keep each endpoint's column tied to its own
/// row (correct for all four drag directions). Spinner and fold rows are
/// skipped so the live indicator and collapsed-group summaries never
/// pollute the copied text.
pub fn extract_text(rows: &[(u8, String)], rect: Rect, sel: &Selection) -> String {
    let Some(((start_col, start_row), (end_col, end_row))) = sel.bounds() else {
        return String::new();
    };
    let mut out = String::new();
    let mut wrote = false;
    for ri in start_row..=end_row {
        let Some((tag, row)) = rows.get(ri) else {
            continue;
        };
        if is_non_selectable(*tag) {
            continue;
        }
        let row_w = UnicodeWidthStr::width(row.as_str());
        let (col_start, col_end) = if start_row == end_row {
            let c0 = start_col.min(end_col).saturating_sub(rect.x) as usize;
            let c1 = start_col.max(end_col).saturating_sub(rect.x) as usize + 1;
            (c0, c1)
        } else if ri == start_row {
            (start_col.saturating_sub(rect.x) as usize, row_w)
        } else if ri == end_row {
            (0, end_col.saturating_sub(rect.x) as usize + 1)
        } else {
            (0, row_w)
        };
        if wrote {
            out.push('\n');
        }
        let glyph_skip = if *tag == TAG_AGENT_FIRST {
            2
        } else if matches!(*tag, TAG_DIFF_ADD | TAG_DIFF_DEL | TAG_DIFF_CTX) {
            diff_gutter_skip(row)
        } else {
            0
        };
        let col_start = col_start.max(glyph_skip);
        out.push_str(&substring_by_display(row, col_start, col_end));
        wrote = true;
    }
    out
}

/// Slice a string by display columns [c0, c1): walk graphemes, accumulate
/// display width, keep graphemes that fall in the range.
fn substring_by_display(s: &str, c0: usize, c1: usize) -> String {
    let mut w = 0usize;
    let mut out = String::new();
    for g in s.graphemes(true) {
        if w >= c1 {
            break;
        }
        let gw = UnicodeWidthStr::width(g);
        if w + gw > c0 {
            out.push_str(g);
        }
        w += gw;
    }
    out
}

/// 3-class grapheme classifier (space / word / other-punct) — a word boundary
/// is a class change. Word chars include path-friendly punctuation so a token
/// like ~/.claude/config.json selects whole.
fn char_class(g: &str) -> u8 {
    if g.chars().all(|c| c.is_whitespace()) {
        0
    } else if g.chars().any(|c| {
        c.is_alphanumeric()
            || c == '_'
            || c == '/'
            || c == '.'
            || c == '-'
            || c == '+'
            || c == '~'
            || c == '\\'
    }) {
        1
    } else {
        2
    }
}

/// The display-column bounds [lo, hi) of the same-class grapheme run at
/// the column — a word-bounds lookup: word chars, punctuation
/// AND whitespace each form a run, so a double-click on spaces selects the
/// whitespace run (not a single cell) and drags extend symmetrically in
/// both directions. A column beyond the text end selects the WHOLE blank
/// tail out to max_w (the viewport width) in one click — the screen
/// buffer holds real blank cells there, so a word select grabs
/// the entire stretch; never snapping back onto the last word.
fn word_bounds_at(row: &str, col: usize, max_w: usize) -> (usize, usize) {
    let graphemes: Vec<&str> = row.graphemes(true).collect();
    // display-col start of each grapheme.
    let mut starts = Vec::with_capacity(graphemes.len());
    let mut widths = Vec::with_capacity(graphemes.len());
    let mut w = 0usize;
    for g in &graphemes {
        starts.push(w);
        let gw = UnicodeWidthStr::width(*g);
        widths.push(gw);
        w += gw;
    }
    // Beyond the text end (or an empty row): the blank tail is one
    // whitespace run from where the text stops out to the viewport width.
    // The trailing-space run of the text joins it (same class).
    if col >= w {
        let mut lo = w;
        let mut i = graphemes.len();
        while i > 0 && char_class(graphemes[i - 1]) == 0 {
            i -= 1;
            lo = starts[i];
        }
        return (lo, max_w.max(col + 1));
    }
    // index of the grapheme covering col (start <= col < start + width)
    let idx = (0..graphemes.len())
        .find(|&i| starts[i] + widths[i] > col)
        .unwrap_or(graphemes.len() - 1);
    // Wide glyphs (CJK) are each their own word: a double-click selects
    // the single character — CJK has no space-
    // delimited word boundaries, so run expansion grabbed whole phrases.
    if widths[idx] >= 2 {
        return (starts[idx], starts[idx] + widths[idx]);
    }
    let cls = char_class(graphemes[idx]);
    let mut lo = idx;
    while lo > 0 && char_class(graphemes[lo - 1]) == cls {
        lo -= 1;
    }
    let mut hi = idx;
    while hi + 1 < graphemes.len() && char_class(graphemes[hi + 1]) == cls {
        hi += 1;
    }
    // A whitespace run at the text end extends through the blank tail
    // (the screen buffer has real blank cells there; our row
    // strings stop at the text, so extend the run to the click column).
    (starts[lo], starts[hi] + widths[hi])
}

pub mod surface;
#[cfg(test)]
mod tests;
