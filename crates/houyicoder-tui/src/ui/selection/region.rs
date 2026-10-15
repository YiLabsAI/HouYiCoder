//! In-app selection regions: the transcript and the slash-command panes
//! (/permissions /search /memory) each render text the user can drag-select
//! and copy. Rather than duplicate the mouse glue per region, a Region
//! trait owns the per-region geometry + policy and default-methods the shared
//! gesture mechanics. A new pure-selection region (a future artifact or
//! approval-args pane) is one impl with zero gesture code — it implements
//! parts (where its rect/rows live), to_content, and is_dragging, and
//! inherits the down/drag/up/moved defaults.
//!
//! The single parts accessor is load-bearing: a region cannot expose
//! sel(&mut self) and rows(&self) separately because the default methods
//! would then hold &mut self and &self at once. parts(&mut self) borrows
//! the disjoint Ui fields (the selection, the frame's row cache and rect,
//! the clipboard) in one shot and hands them out together, so a default
//! method body can drive the shared free functions without a borrow
//! conflict. The fields are disjoint — selection vs last_all_rows vs
//! clipboard — and split-borrowing them through &mut App is the same
//! pattern the prior inline glue already relied on.

use std::cell::Ref;

use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Color,
};

use crate::renderer::PreparedFrame;
use crate::selection::{self, ClipboardWriter, Selection, extract_text, is_non_selectable};
use crate::selections::Selections;
use crate::state::App;

// ---- shared free functions (pure: no App) -------------------------------

/// Apply a click to a selection by multi-click count: char / word / line.
/// Pure over the selection + its row stash + rect; the surface owns the
/// mouse→content mapping and any pre-click interaction (fold toggle).
pub(crate) fn apply_click(
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
pub(crate) fn extend_drag(
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
pub(crate) fn finish_release(
    sel: &mut Selection,
    rect: Rect,
    rows: &[(u8, String)],
    clipboard: &dyn ClipboardWriter,
    persist: bool,
) -> Option<String> {
    sel.finish();
    sel.cursor = None;
    if sel.drag_moved {
        sel.last_click = None;
    }
    if sel.is_click_only() && sel.span_origin.is_none() {
        sel.clear();
        return None;
    }
    let text = extract_text(rows, rect, sel);
    if !text.trim().is_empty() {
        clipboard.write(&text);
        if !persist {
            sel.clear();
        }
        return Some(text);
    }
    if !persist {
        sel.clear();
    }
    None
}

/// Cancel a click-only drag before a scroll. A left-down always starts a drag
/// so the focus follows the cursor; if the terminal never delivers a clean Up
/// the stale one-cell selection would otherwise extend into scrolled content.
/// A real drag (anchor != focus) is left alone so the user can scroll while
/// holding an active selection.
pub(crate) fn clear_stale_click(sel: &mut Selection) {
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
pub(crate) fn transcript_mouse_to_content(
    frame: &PreparedFrame,
    scroll_top: usize,
    x: u16,
    y: u16,
) -> (u16, usize) {
    let rect = frame.layout.transcript_rect.get();
    // The rows the frame drew are the total: the same set the hit test maps
    // into, so the clamp cannot disagree with the drawing.
    let total = frame.layout.last_all_rows.borrow().len();
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

// ---- the Region trait + default mechanics --------------------------------

/// A one-shot borrow of a surface's disjoint fields: the mutable Selection,
/// its row stash (a RefCell guard), its screen rect, and the clipboard writer.
/// Returned by Region::parts so a default trait method can drive the shared
/// free functions without holding &mut self and &self at once.
pub(crate) struct RegionParts<'a> {
    pub sel: &'a mut Selection,
    pub rows: Ref<'a, Vec<(u8, String)>>,
    pub rect: Rect,
    pub clipboard: &'a dyn ClipboardWriter,
}

/// Everything a selection leaf may read: the four per-region selections (a
/// leaf takes its own), the last successfully drawn frame, the transcript's
/// current top display row, and the clipboard port. No App handle, so a leaf
/// cannot reach domain state even by accident; the effects a gesture asks for
/// come back as a RegionIntent for the router to perform.
///
/// The top is the live offset rather than the one the frame was drawn from:
/// an edge scroll moves it and the same gesture then maps through the new
/// value, so the focus row is the one the next render will show under the
/// cursor.
pub(crate) struct RegionView<'a> {
    pub selections: &'a mut Selections,
    pub frame: &'a PreparedFrame,
    pub scroll_top: usize,
    pub clipboard: &'a dyn ClipboardWriter,
}

/// What a gesture asks the router to do beyond the shared mechanics. The leaf
/// cannot perform these itself: they change state the render pass publishes
/// (folds, expansion, the scroll offset) or raise a notice, and the router
/// owns that coordination.
#[derive(Debug)]
pub(crate) enum RegionIntent {
    /// The gesture only moved the selection.
    None,
    /// Toggle the fold group whose summary is the visible row.
    ToggleFold(usize),
    /// Toggle the delegation expansion on the visible row.
    ToggleSubagent(usize),
    /// Toggle the thinking block on the visible row, when it offers one.
    ToggleThinking(usize),
    /// Collapse whatever expanded block contains the selection anchor.
    CollapseUnderAnchor,
    /// Scroll one line up (edge auto-scroll during a drag).
    ScrollUp,
    /// Scroll one line down.
    ScrollDown,
    /// Raise the copy notice for the text a release copied.
    Copied(String),
}

/// A draggable, copyable text surface. The transcript and each slash-command
/// pane implement this so the mouse router is one path: the router picks the
/// active surface by rect (Down) or by which is dragging (Drag/Up/Moved) and
/// dispatches. Default methods own the shared gesture mechanics; a surface
/// overrides only what it genuinely does differently (the transcript adds
/// fold/thinking click intercepts, edge auto-scroll, and a clean-click
/// collapse guard; a pure-selection pane overrides nothing).
pub(crate) trait Region {
    /// One-shot borrow of the disjoint fields the default mechanics need.
    fn parts(&mut self) -> RegionParts<'_>;
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
    fn handle_down(&mut self, x: u16, y: u16) -> RegionIntent {
        down_body(self, x, y);
        RegionIntent::None
    }
    fn handle_drag(&mut self, x: u16, y: u16) -> RegionIntent {
        drag_body(self, x, y);
        RegionIntent::None
    }
    /// The release mechanics: finish the gesture, copy what was selected.
    /// A surface that intercepts a clean click for a toggle calls this when
    /// no toggle applied, so the click still clears its caret.
    fn release(&mut self) -> RegionIntent {
        let persist = self.persist();
        let p = self.parts();
        match finish_release(p.sel, p.rect, &p.rows, p.clipboard, persist) {
            Some(text) => RegionIntent::Copied(text),
            None => RegionIntent::None,
        }
    }
    fn handle_up(&mut self) -> RegionIntent {
        self.release()
    }
    fn handle_moved(&mut self) -> RegionIntent {
        self.handle_up()
    }

    /// Copy the current selection without finishing or clearing it (the
    /// ctrl+C path). No-op when there is no range. The highlight stays as an
    /// independent visual affordance — it clears on Esc or the next click,
    /// not on copy. Returns the copied text so the caller can show a toast.
    fn copy_current(&mut self) -> RegionIntent {
        let p = self.parts();
        if !p.sel.has_selection() {
            return RegionIntent::None;
        }
        let text = extract_text(&p.rows, p.rect, p.sel);
        if text.trim().is_empty() {
            return RegionIntent::None;
        }
        p.clipboard.write(&text);
        RegionIntent::Copied(text)
    }
}

/// Shared down body: lost-release recovery (direct finish_release), then
/// on_click, set the cursor, apply the click. Used by the trait default and
/// by surfaces that prepend their own click intercepts (the transcript).
fn down_body<S: Region + ?Sized>(s: &mut S, x: u16, y: u16) {
    let persist = s.persist();
    let was_dragging = s.is_dragging();
    let (col, cr) = s.to_content(x, y);
    let p = s.parts();
    if was_dragging {
        let _text = finish_release(p.sel, p.rect, &p.rows, p.clipboard, persist);
    }
    let count = p.sel.on_click(x, y);
    p.sel.cursor = Some((x, y));
    apply_click(p.sel, &p.rows, p.rect, col, cr, count);
}

/// Shared drag body: gate on is_dragging, map to content, set the cursor,
/// extend the selection. Used by the trait default and by surfaces that
/// prepend edge auto-scroll (the transcript, which re-maps to content after
/// the scroll).
fn drag_body<S: Region + ?Sized>(s: &mut S, x: u16, y: u16) {
    if !s.is_dragging() {
        return;
    }
    let (col, cr) = s.to_content(x, y);
    let p = s.parts();
    p.sel.cursor = Some((x, y));
    extend_drag(p.sel, &p.rows, p.rect, col, cr);
}

/// The edge auto-scroll a drag onto the transcript's top or bottom row asks
/// for, or None when the drag is inside the viewport. Pure over the drawn
/// rect: the router applies it BEFORE the drag body so the focus row is
/// mapped through the post-scroll offset and matches what the next render
/// shows under the cursor.
pub(crate) fn edge_scroll_intent(rect: Rect, y: u16) -> RegionIntent {
    if rect.height == 0 {
        RegionIntent::None
    } else if y <= rect.y {
        RegionIntent::ScrollUp
    } else if y >= rect.y + rect.height.saturating_sub(1) {
        RegionIntent::ScrollDown
    } else {
        RegionIntent::None
    }
}

// ---- the transcript surface ----------------------------------------------

/// The transcript selection surface: content-space, scrollable, with rich
/// click interaction (fold toggle, thinking-toggle, clean-click collapse) and
/// edge auto-scroll. Overrides the three gesture methods to layer those on the
/// shared mechanics; persist is true so the highlight survives a copy.
pub(crate) struct TranscriptRegion<'a> {
    pub view: RegionView<'a>,
}

impl Region for TranscriptRegion<'_> {
    fn parts(&mut self) -> RegionParts<'_> {
        let layout = &self.view.frame.layout;
        let rows = layout.last_all_rows.borrow();
        RegionParts {
            sel: &mut self.view.selections.transcript,
            rows,
            rect: layout.transcript_rect.get(),
            clipboard: self.view.clipboard,
        }
    }
    fn to_content(&self, x: u16, y: u16) -> (u16, usize) {
        transcript_mouse_to_content(self.view.frame, self.view.scroll_top, x, y)
    }
    fn persist(&self) -> bool {
        true
    }
    fn is_dragging(&self) -> bool {
        self.view.selections.transcript.is_dragging
    }

    /// Press always starts a selection (the shared down body): a fold,
    /// subagent, or thought toggle is decided on release, not here, so a drag
    /// can select the summary text instead of being swallowed by the toggle.
    /// The clean-click toggle dispatch lives in handle_up.
    fn handle_down(&mut self, x: u16, y: u16) -> RegionIntent {
        down_body(self, x, y);
        RegionIntent::None
    }

    /// A clean click (no drag motion) asks for a fold/subagent/thought toggle,
    /// or a collapse when it lands on an expanded block's content; a real drag
    /// copies (drag wins over toggle). The leaf only names the effect: the
    /// row's tag, fold key, and turn handle decide which, and the router
    /// performs it on the state the render pass publishes.
    fn handle_up(&mut self) -> RegionIntent {
        let sel = &self.view.selections.transcript;
        let clean = !sel.drag_moved && sel.is_click_only() && sel.span_origin.is_none();
        if clean {
            let layout = &self.view.frame.layout;
            let target = self.anchor_visible_row().map(|ri| {
                let tag = layout
                    .last_transcript_rows
                    .borrow()
                    .get(ri)
                    .map(|(t, _)| *t)
                    .unwrap_or(selection::TAG_PLAIN);
                let keys = self.view.frame.keys.borrow();
                (ri, tag, keys.fold_key_at(ri), keys.turn_id_at(ri))
            });
            if let Some((ri, tag, fold_key, turn_id)) = target {
                if tag == selection::TAG_FOLD {
                    return RegionIntent::ToggleFold(ri);
                }
                if fold_key.is_some() {
                    return RegionIntent::ToggleSubagent(ri);
                }
                // A row publishes a turn handle exactly when it offers a
                // toggle, so the handle is the row's shape. Matching the
                // rendered text instead missed every summary whose duration
                // is unknown, because that text names a duration the row
                // cannot state.
                if turn_id.is_some() {
                    return RegionIntent::ToggleThinking(ri);
                }
            }
            return RegionIntent::CollapseUnderAnchor;
        }
        self.release()
    }
}

impl TranscriptRegion<'_> {
    /// The visible row the selection anchor sits on, or None when it scrolled
    /// off. Maps through the same top the caller's gesture used, so the row it
    /// names is the one that top was applied to.
    fn anchor_visible_row(&self) -> Option<usize> {
        let (_, content_row) = self.view.selections.transcript.anchor?;
        let ri = content_row.checked_sub(self.view.scroll_top)?;
        let height = self.view.frame.layout.transcript_rect.get().height as usize;
        (ri < height).then_some(ri)
    }
}

// ---- the pane surface ----------------------------------------------------

/// A slash-command pane selection surface: screen-space (no independent
/// scroll), pure selection. Implements only parts, to_content, and
/// is_dragging — all gesture mechanics come from the trait defaults. A
/// future artifact or approval-args pane is a peer of this.
pub(crate) struct PaneRegion<'a> {
    pub view: RegionView<'a>,
}

impl Region for PaneRegion<'_> {
    fn parts(&mut self) -> RegionParts<'_> {
        let layout = &self.view.frame.layout;
        let rows = layout.last_pane_rows.borrow();
        RegionParts {
            sel: &mut self.view.selections.pane,
            rows,
            rect: layout.pane_rect.get(),
            clipboard: self.view.clipboard,
        }
    }
    fn to_content(&self, x: u16, y: u16) -> (u16, usize) {
        pane_mouse_to_content(self.view.frame.layout.pane_rect.get(), x, y)
    }
    fn is_dragging(&self) -> bool {
        self.view.selections.pane.is_dragging
    }
}

/// The approval card selection handler: screen-space (no independent
/// scroll), pure selection — the card is a transient layout slot, so it
/// uses persist=false (the card rebuilds each frame; a stale highlight
/// would not track the rows). Mirrors PaneRegion; the only difference is
/// which rect/rows/selection it binds.
pub(crate) struct ApprovalSelection<'a> {
    pub view: RegionView<'a>,
}

impl Region for ApprovalSelection<'_> {
    fn parts(&mut self) -> RegionParts<'_> {
        let layout = &self.view.frame.layout;
        let rows = layout.last_approval_rows.borrow();
        RegionParts {
            sel: &mut self.view.selections.approval,
            rows,
            rect: layout.approval_rect.get(),
            clipboard: self.view.clipboard,
        }
    }
    fn to_content(&self, x: u16, y: u16) -> (u16, usize) {
        pane_mouse_to_content(self.view.frame.layout.approval_rect.get(), x, y)
    }
    fn is_dragging(&self) -> bool {
        self.view.selections.approval.is_dragging
    }
}

/// The status bar selection surface: a single chrome row (model + mode +
/// context gauge) that the user can drag-select + copy so bug reports can
/// quote the status text verbatim. Pure selection — no fold or click
/// intercepts — so the trait defaults drive every gesture. The rows are
/// read back from the frame buffer (see stash_status_rows) so the copyable
/// text is exactly what the bar drew. The highlight clears on release
/// (persist = false) because the bar rebuilds each frame.
pub(crate) struct StatusRegion<'a> {
    pub view: RegionView<'a>,
}

impl Region for StatusRegion<'_> {
    fn parts(&mut self) -> RegionParts<'_> {
        let layout = &self.view.frame.layout;
        let rows = layout.last_status_rows.borrow();
        RegionParts {
            sel: &mut self.view.selections.status,
            rows,
            rect: layout.status_rect.get(),
            clipboard: self.view.clipboard,
        }
    }
    fn to_content(&self, x: u16, y: u16) -> (u16, usize) {
        pane_mouse_to_content(self.view.frame.layout.status_rect.get(), x, y)
    }
    fn is_dragging(&self) -> bool {
        self.view.selections.status.is_dragging
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
    if !app.ui.selection.transcript.is_dragging {
        return false;
    }
    let Some((cx, cy)) = app.ui.selection.transcript.cursor else {
        return false;
    };
    let rect = app.ui.frame.layout.transcript_rect.get();
    if rect.height == 0 {
        return false;
    }
    match edge_scroll_intent(rect, cy) {
        RegionIntent::ScrollUp => app.scroll_transcript_line_up(1),
        RegionIntent::ScrollDown => app.scroll_transcript_line_down(1),
        _ => return false,
    }
    let top = app.active_scroll().top_offset();
    let (col, content_row) = transcript_mouse_to_content(&app.ui.frame, top, cx, cy);
    let rows = app.ui.frame.layout.last_all_rows.borrow();
    extend_drag(
        &mut app.ui.selection.transcript,
        &rows,
        rect,
        col,
        content_row,
    );
    true
}

#[cfg(test)]
mod tests;
