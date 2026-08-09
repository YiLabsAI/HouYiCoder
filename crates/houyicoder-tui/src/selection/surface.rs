//! In-app selection surfaces: the transcript and the slash-command panes
//! (/permissions /search /memory) each render text the user can drag-select
//! and copy. Rather than duplicate the mouse glue per surface, a Surface
//! trait owns the per-surface geometry + policy and default-methods the shared
//! gesture mechanics. A new pure-selection surface (a future artifact or
//! approval-args pane) is one impl with zero gesture code — it implements
//! parts (where its rect/rows live), to_content, and is_dragging, and
//! inherits the down/drag/up/moved defaults.
//!
//! The single parts accessor is load-bearing: a surface cannot expose
//! sel(&mut self) and rows(&self) separately because the default methods
//! would then hold &mut self and &self at once. parts(&mut self) borrows
//! the disjoint App fields (the Selection, its row RefCell, the rect Cell, the
//! clipboard) in one shot and hands them out together, so a default method
//! body can drive the shared free functions without a borrow conflict. The
//! fields are disjoint — selection vs last_all_rows vs clipboard — and
//! split-borrowing them through &mut App is the same pattern the prior
//! inline glue already relied on.

use std::cell::Ref;

use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Color,
};

use crate::selection::{self, ClipboardWriter, Selection, extract_text, is_non_selectable};
use crate::state::App;

// ---- shared free functions (pure: no App) -------------------------------

/// Apply a click to a selection by multi-click count: char / word / line.
/// Pure over the selection + its row stash + rect; the surface owns the
/// mouse→content mapping and any pre-click interaction (fold toggle).
pub fn apply_click(
    sel: &mut Selection,
    rows: &[(u8, String)],
    rect: Rect,
    col: u16,
    content_row: usize,
    count: u8,
) {
    match count {
        2 => sel.select_word(rows, rect, col, content_row),
        3 => sel.select_line(rect, content_row),
        _ => sel.start(col, content_row),
    }
}

/// Extend a drag focus: promote a blank-anchor drag to a word span, then
/// extend the span or update the char-mode focus. Pure over selection + rows
/// + rect; the surface owns cursor bookkeeping and edge auto-scroll.
pub fn extend_drag(
    sel: &mut Selection,
    rows: &[(u8, String)],
    rect: Rect,
    col: u16,
    content_row: usize,
) {
    sel.promote_blank_anchor(rows, rect);
    if sel.span_origin.is_some() {
        sel.extend_span(rows, rect, col, content_row);
    } else {
        sel.update(col, content_row);
    }
}

/// End a drag gesture: finish, break the multi-click chain after a moved
/// gesture, clear a click-only remnant, else copy the selected text and (only
/// when persist is false) clear the range. persist is the single axis that
/// distinguishes the transcript (keep the highlight after copy) from a pane
/// (clear on release, since pane content rebuilds each frame and a stale
/// highlight would not track the rows).
pub fn finish_release(
    sel: &mut Selection,
    rect: Rect,
    rows: &[(u8, String)],
    clipboard: &dyn ClipboardWriter,
    persist: bool,
) {
    sel.finish();
    sel.cursor = None;
    if sel.drag_moved {
        sel.last_click = None;
    }
    if sel.is_click_only() && sel.span_origin.is_none() {
        sel.clear();
    } else {
        let text = extract_text(rows, rect, sel);
        if !text.trim().is_empty() {
            clipboard.write(&text);
        }
        if !persist {
            sel.clear();
        }
    }
}

/// Cancel a click-only drag before a scroll. A left-down always starts a drag
/// so the focus follows the cursor; if the terminal never delivers a clean Up
/// the stale one-cell selection would otherwise extend into scrolled content.
/// A real drag (anchor != focus) is left alone so the user can scroll while
/// holding an active selection.
pub fn clear_stale_click(sel: &mut Selection) {
    if sel.is_dragging && sel.is_click_only() && sel.span_origin.is_none() {
        sel.clear();
    }
}

// ---- mouse → content mapping (per surface) ------------------------------

/// The screen row of the last visible content row. When the transcript has
/// fewer rows than the viewport (content tail above the rect bottom), blank
/// rows fill the tail — clicking or dragging there would put the anchor on a
/// non-existent content row. Returns the screen row of the last real content
/// row so the caller can clamp to it instead of the rect bottom. Falls back
/// to the rect bottom when the content fills the viewport.
pub(crate) fn last_visible_content_row(rect: Rect, total: usize, scroll_top: usize) -> u16 {
    let visible = total.saturating_sub(scroll_top).min(rect.height as usize);
    rect.y + visible.saturating_sub(1) as u16
}

/// Map a transcript mouse position to content space: clamp to the transcript
/// rect and to the last visible content row (the rect can be one frame stale
/// and includes blank rows below the content tail), then map the screen row
/// through the current scroll offset. The column stays in screen space
/// (columns map 1:1 to content; there is no horizontal scroll).
pub(crate) fn transcript_mouse_to_content(app: &App, rect: Rect, x: u16, y: u16) -> (u16, usize) {
    let total = app.transcript_scroll.total.get();
    let scroll_top = app.transcript_scroll.top_offset(total);
    let col = x.clamp(rect.x, rect.x + rect.width.saturating_sub(1));
    let last_content = last_visible_content_row(rect, total, scroll_top);
    let row = y
        .clamp(rect.y, last_content)
        .min(rect.y + rect.height.saturating_sub(1));
    let content_row = scroll_top + (row as usize).saturating_sub(rect.y as usize);
    (col, content_row)
}

/// Map a pane mouse position to content space. The pane does not scroll
/// independently, so the content row is a direct index into its row stash:
/// the screen row offset from the pane rect top. The column stays in screen
/// space.
pub(crate) fn pane_mouse_to_content(rect: Rect, x: u16, y: u16) -> (u16, usize) {
    let col = x.clamp(rect.x, rect.x + rect.width.saturating_sub(1));
    let row = (y.saturating_sub(rect.y)) as usize;
    (col, row)
}

// ---- the Surface trait + default mechanics --------------------------------

/// A one-shot borrow of a surface's disjoint fields: the mutable Selection,
/// its row stash (a RefCell guard), its screen rect, and the clipboard writer.
/// Returned by Surface::parts so a default trait method can drive the shared
/// free functions without holding &mut self and &self at once.
pub struct SurfaceParts<'a> {
    pub sel: &'a mut Selection,
    pub rows: Ref<'a, Vec<(u8, String)>>,
    pub rect: Rect,
    pub clipboard: &'a dyn ClipboardWriter,
}

/// A draggable, copyable text surface. The transcript and each slash-command
/// pane implement this so the mouse router is one path: the router picks the
/// active surface by rect (Down) or by which is dragging (Drag/Up/Moved) and
/// dispatches. Default methods own the shared gesture mechanics; a surface
/// overrides only what it genuinely does differently (the transcript adds
/// fold/thinking click intercepts, edge auto-scroll, and a clean-click
/// collapse guard; a pure-selection pane overrides nothing).
pub trait Surface {
    /// One-shot borrow of the disjoint fields the default mechanics need.
    fn parts(&mut self) -> SurfaceParts<'_>;
    /// Map a screen cell to this surface's content space. &self only; call
    /// before parts so the shared borrow does not overlap the parts borrow.
    fn to_content(&self, x: u16, y: u16) -> (u16, usize);
    /// Whether the highlight persists after a copy-on-release. The transcript
    /// keeps it (matching an editor); a pane clears it because its
    /// content rebuilds each frame.
    fn persist(&self) -> bool {
        false
    }
    fn is_dragging(&self) -> bool;

    /// Default down for a pure-selection surface: lost-release recovery (a
    /// fresh press while still dragging finishes the stale gesture directly
    /// via finish_release, NOT handle_up — a surface's handle_up may
    /// carry a clean-click collapse guard that must not fire on a lost
    /// release), then on_click + apply_click.
    fn handle_down(&mut self, x: u16, y: u16) {
        down_body(self, x, y);
    }
    fn handle_drag(&mut self, x: u16, y: u16) {
        drag_body(self, x, y);
    }
    fn handle_up(&mut self) {
        let persist = self.persist();
        let p = self.parts();
        finish_release(p.sel, p.rect, &p.rows, p.clipboard, persist);
    }
    fn handle_moved(&mut self) {
        self.handle_up();
    }

    /// Copy the current selection without finishing or clearing it (the
    /// ctrl+C path). No-op when there is no range. The highlight stays as an
    /// independent visual affordance — it clears on Esc or the next click,
    /// not on copy.
    fn copy_current(&mut self) {
        let p = self.parts();
        if !p.sel.has_selection() {
            return;
        }
        let text = extract_text(&p.rows, p.rect, p.sel);
        if !text.trim().is_empty() {
            p.clipboard.write(&text);
        }
    }
}

/// Shared down body: lost-release recovery (direct finish_release), then
/// on_click, set the cursor, apply the click. Used by the trait default and
/// by surfaces that prepend their own click intercepts (the transcript).
fn down_body<S: Surface + ?Sized>(s: &mut S, x: u16, y: u16) {
    let persist = s.persist();
    let was_dragging = s.is_dragging();
    let (col, cr) = s.to_content(x, y);
    let p = s.parts();
    if was_dragging {
        finish_release(p.sel, p.rect, &p.rows, p.clipboard, persist);
    }
    let count = p.sel.on_click(x, y);
    p.sel.cursor = Some((x, y));
    apply_click(p.sel, &p.rows, p.rect, col, cr, count);
}

/// Shared drag body: gate on is_dragging, map to content, set the cursor,
/// extend the selection. Used by the trait default and by surfaces that
/// prepend edge auto-scroll (the transcript, which re-maps to content after
/// the scroll).
fn drag_body<S: Surface + ?Sized>(s: &mut S, x: u16, y: u16) {
    if !s.is_dragging() {
        return;
    }
    let (col, cr) = s.to_content(x, y);
    let p = s.parts();
    p.sel.cursor = Some((x, y));
    extend_drag(p.sel, &p.rows, p.rect, col, cr);
}

// ---- the transcript surface ----------------------------------------------

/// The transcript selection surface: content-space, scrollable, with rich
/// click interaction (fold toggle, thinking-toggle, clean-click collapse) and
/// edge auto-scroll. Overrides the three gesture methods to layer those on the
/// shared mechanics; persist is true so the highlight survives a copy.
pub struct TranscriptSurface<'a> {
    pub app: &'a mut App,
}

impl Surface for TranscriptSurface<'_> {
    fn parts(&mut self) -> SurfaceParts<'_> {
        let rows = self.app.last_all_rows.borrow();
        SurfaceParts {
            sel: &mut self.app.selection,
            rows,
            rect: self.app.transcript_rect.get(),
            clipboard: self.app.clipboard.as_ref(),
        }
    }
    fn to_content(&self, x: u16, y: u16) -> (u16, usize) {
        let rect = self.app.transcript_rect.get();
        transcript_mouse_to_content(self.app, rect, x, y)
    }
    fn persist(&self) -> bool {
        true
    }
    fn is_dragging(&self) -> bool {
        self.app.selection.is_dragging
    }

    /// Fold/thought click intercepts first (a click on a fold summary or
    /// "Thought for" row toggles instead of starting a selection), then the
    /// shared down body. The order: intercept, then
    /// recovery, then on_click.
    fn handle_down(&mut self, x: u16, y: u16) {
        let rect = self.app.transcript_rect.get();
        let ri = (y.saturating_sub(rect.y)) as usize;
        let (tag, text) = self
            .app
            .last_transcript_rows
            .borrow()
            .get(ri)
            .cloned()
            .unwrap_or((selection::TAG_PLAIN, String::new()));
        if tag == selection::TAG_FOLD {
            self.app.toggle_fold_at_row(ri);
            return;
        }
        if text.contains("Thought for") {
            self.app.toggle_thinking_expand_at_row(ri);
            return;
        }
        down_body(self, x, y);
    }

    /// Edge auto-scroll one line per drag event (reaches the top/bottom of
    /// the transcript), then the shared drag body — which re-maps to content
    /// through the POST-scroll offset so the focus row matches what the next
    /// render shows under the cursor.
    fn handle_drag(&mut self, x: u16, y: u16) {
        if !self.app.selection.is_dragging {
            return;
        }
        let rect = self.app.transcript_rect.get();
        if rect.height > 0 {
            if y <= rect.y {
                self.app.scroll_transcript_line_up(1);
            } else if y >= rect.y + rect.height.saturating_sub(1) {
                self.app.scroll_transcript_line_down(1);
            }
        }
        drag_body(self, x, y);
    }

    /// A clean click (no drag motion) inside an expanded fold block collapses
    /// it; a real drag copies (drag wins over collapse). The guard runs before
    /// parts because collapse_expanded_under_anchor needs &mut App
    /// wholesale; parts would split-borrow the App fields.
    fn handle_up(&mut self) {
        if !self.app.selection.drag_moved
            && self.app.selection.is_click_only()
            && self.app.selection.span_origin.is_none()
            && self.app.collapse_expanded_under_anchor()
        {
            return;
        }
        let p = self.parts();
        finish_release(p.sel, p.rect, &p.rows, p.clipboard, true);
    }
}

// ---- the pane surface ----------------------------------------------------

/// A slash-command pane selection surface: screen-space (no independent
/// scroll), pure selection. Implements only parts, to_content, and
/// is_dragging — all gesture mechanics come from the trait defaults. A
/// future artifact or approval-args pane is a peer of this.
pub struct PaneSurface<'a> {
    pub app: &'a mut App,
}

impl Surface for PaneSurface<'_> {
    fn parts(&mut self) -> SurfaceParts<'_> {
        let rows = self.app.last_pane_rows.borrow();
        SurfaceParts {
            sel: &mut self.app.pane_selection,
            rows,
            rect: self.app.pane_rect.get(),
            clipboard: self.app.clipboard.as_ref(),
        }
    }
    fn to_content(&self, x: u16, y: u16) -> (u16, usize) {
        pane_mouse_to_content(self.app.pane_rect.get(), x, y)
    }
    fn is_dragging(&self) -> bool {
        self.app.pane_selection.is_dragging
    }
}

// ---- overlay (draw path) -------------------------------------------------

/// Paint a solid selection background over the surface's cells. The range is
/// walked in CONTENT-row space and mapped to screen rows through scroll_top
/// (the transcript's offset; 0 for a pane, which is screen-space). Non-content
/// rows (spinner, fold summaries, collapse hints) are skipped so they never
/// get a highlight and never pollute copied text. Endpoint columns stay tied
/// to their own rows. Applied after the view draw, before the buffer flushes.
pub(crate) fn paint_overlay(
    buf: &mut Buffer,
    rect: Rect,
    rows: &[(u8, String)],
    sel: &Selection,
    scroll_top: usize,
) {
    let Some(((top_x, top_row), (bot_x, bot_row))) = sel.bounds() else {
        return;
    };
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let vh = rect.height as usize;
    if bot_row < scroll_top || top_row >= scroll_top.saturating_add(vh) {
        return;
    }
    let right = rect.x + rect.width - 1;
    let sel_bg = Color::Indexed(24);
    let first = top_row.max(scroll_top);
    let last = bot_row.min(scroll_top + vh - 1);
    for cr in first..=last {
        let ri = cr - scroll_top;
        match rows.get(ri) {
            Some((t, _)) if is_non_selectable(*t) => continue,
            None => continue,
            _ => {}
        }
        let y = rect.y + ri as u16;
        let (sx, ex) = if top_row == bot_row {
            (top_x.min(bot_x), top_x.max(bot_x))
        } else if cr == top_row {
            (top_x, right)
        } else if cr == bot_row {
            (rect.x, bot_x)
        } else {
            (rect.x, right)
        };
        let sx = sx.max(rect.x).min(right);
        let ex = ex.max(rect.x).min(right);
        for x in sx..=ex {
            if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                cell.bg = sel_bg;
            }
        }
    }
}

// ---- edge auto-scroll (poll tick, transcript) ----------------------------

/// Continuous edge auto-scroll during a transcript drag. Driven from the
/// run-loop poll tick (about every 100ms when no input event fires): while the
/// user holds the mouse at the top or bottom edge of the transcript no new drag
/// event arrives, so this scrolls one line in the edge direction and re-derives
/// the focus content row from the cached cursor position plus the post-scroll
/// offset. No-op when the cursor is not at an edge or no drag is active.
pub(crate) fn edge_scroll_if_at_edge(app: &mut App) -> bool {
    if !app.selection.is_dragging {
        return false;
    }
    let Some((cx, cy)) = app.selection.cursor else {
        return false;
    };
    let rect = app.transcript_rect.get();
    if rect.height == 0 {
        return false;
    }
    let bottom = rect.y + rect.height.saturating_sub(1);
    if cy <= rect.y {
        app.scroll_transcript_line_up(1);
    } else if cy >= bottom {
        app.scroll_transcript_line_down(1);
    } else {
        return false;
    }
    let (col, content_row) = transcript_mouse_to_content(app, rect, cx, cy);
    let rows = app.last_all_rows.borrow();
    extend_drag(&mut app.selection, &rows, rect, col, content_row);
    true
}

#[cfg(test)]
#[path = "surface_tests.rs"]
mod tests;
