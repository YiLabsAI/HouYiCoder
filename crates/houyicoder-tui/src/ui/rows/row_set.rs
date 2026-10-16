//! The transcript rows and the metadata each row carries, held as one set that
//! stays in step.
//!
//! A click or key resolves a screen row to the call id, fold key, group, or
//! turn it acts on, so the row text and its metadata have to travel together.
//! Separate arrays let a write land on one and not the others, and the reader
//! then acts on a row beside the one it points at. One value makes that
//! unrepresentable: the only writers are push and the batch replace on the
//! caches, and both take the text and the metadata in the same operation.
//!
//! The row's identity travels with them for the same reason: a parked viewport
//! names the row it stands on, converts that name back to a screen row when the
//! list is rebuilt, and needs the text of the rows around it to keep the offset
//! into the row's own text. Rows of one line share the identity, so the run of
//! rows carrying one name is what the offset counts over.

use std::ops::Range;

use ratatui::text::Line;

use crate::records::ToolOutcome;
use crate::row_id::RowId;
use crate::scroll::ScrollAnchor;
use crate::selection::TAG_PLAIN;

/// One row: the tag its styling keys on, its plain text, and the tool outcome
/// the styled fallback needs when the row carries no pre-rendered line.
pub(crate) type RowText = (u8, String, Option<ToolOutcome>);

/// A row's text plus the metadata a later click or key resolves by row index.
pub(crate) struct Row {
    tag: u8,
    text: String,
    outcome: Option<ToolOutcome>,
    callid: Option<String>,
    fold_key: Option<String>,
    group: Option<String>,
    turn_id: Option<RowId>,
    pre: Option<Line<'static>>,
}

impl Row {
    pub(crate) fn new(tag: u8, text: impl Into<String>) -> Self {
        Self {
            tag,
            text: text.into(),
            outcome: None,
            callid: None,
            fold_key: None,
            group: None,
            turn_id: None,
            pre: None,
        }
    }

    /// The blank spacer between sections. Plain by construction: a content
    /// tag would drag that tag's background across the gap.
    pub(crate) fn spacer() -> Self {
        Self::new(TAG_PLAIN, String::new())
    }

    pub(crate) fn outcome(mut self, outcome: Option<ToolOutcome>) -> Self {
        self.outcome = outcome;
        self
    }

    /// The result call id, so Ctrl+O on this row expands that result.
    pub(crate) fn callid(mut self, callid: Option<String>) -> Self {
        self.callid = callid;
        self
    }

    /// The group key or child session id this row is an expand handle for.
    pub(crate) fn fold_key(mut self, key: Option<String>) -> Self {
        self.fold_key = key;
        self
    }

    /// The expanded group this row sits inside, which paints the block
    /// background across the region.
    pub(crate) fn group(mut self, group: Option<String>) -> Self {
        self.group = group;
        self
    }

    /// The reasoning turn id, set on a thought header so a click resolves
    /// straight to the turn rather than counting rows.
    pub(crate) fn turn_id(mut self, turn_id: Option<RowId>) -> Self {
        self.turn_id = turn_id;
        self
    }

    /// A styled line, for styling the tag cannot express (markdown, diffs,
    /// badge colors).
    pub(crate) fn pre(mut self, pre: Option<Line<'static>>) -> Self {
        self.pre = pre;
        self
    }
}

/// The rows of one view and the per-row metadata, kept in step by
/// construction.
///
/// The fields are private: a reader takes a slice, a text pair, or the whole
/// set, and a write goes through push or the batch replace on the caches. The
/// delegation nesting flag is builder state that lives here because the
/// builder fills the same value it hands out; a wrapper would be a second
/// type for one bool.
#[derive(Debug, Default)]
pub(crate) struct RowSet {
    rows: Vec<RowText>,
    callids: Vec<Option<String>>,
    fold_keys: Vec<Option<String>>,
    expanded_group: Vec<Option<String>>,
    turn_ids: Vec<Option<RowId>>,
    row_ids: Vec<Option<RowId>>,
    pre_rendered: Vec<Option<Line<'static>>>,
    in_subagent: bool,
    /// The identity the rows pushed from here on belong to, until the next
    /// line begins. Builder state, like the delegation nesting flag: a line
    /// names itself once and every row it emits takes that name -- the blank
    /// row separating it from the one above included, and every row of a call
    /// beside its result, which is what makes those rows one run.
    line_id: Option<RowId>,
}

impl RowSet {
    /// Append one row and its metadata. The arrays move together, so a row
    /// cannot arrive without the keys a later click resolves against it, nor
    /// without the identity a parked viewport names it by.
    pub(crate) fn push(&mut self, row: Row) {
        self.rows.push((row.tag, row.text, row.outcome));
        self.callids.push(row.callid);
        self.fold_keys.push(row.fold_key);
        self.expanded_group.push(row.group);
        self.turn_ids.push(row.turn_id);
        self.row_ids.push(self.line_id.clone());
        self.pre_rendered.push(row.pre);
    }

    /// Begin a line: the rows pushed from here on belong to its identity until
    /// the next call. The caller sets it once per line and the rows the line
    /// emits all carry it.
    pub(crate) fn begin_line(&mut self, id: RowId) {
        self.line_id = Some(id);
    }

    /// True while emitting the rows of an expanded delegation. Row builders
    /// read it to leave out their own expand affordances: a delegation's
    /// block is already one expanded thing, and every row inside advertising
    /// its own toggle turns a summary into a wall of hints nested one level
    /// deeper than the block the user opened.
    pub(crate) fn in_subagent(&self) -> bool {
        self.in_subagent
    }

    /// Emit rows as the inside of a delegation. Saves and restores the flag
    /// rather than clearing it, so a nested delegation does not hand the
    /// outer one back its parent's context on the way out.
    pub(crate) fn within_subagent<R>(&mut self, emit: impl FnOnce(&mut Self) -> R) -> R {
        let outer = self.in_subagent;
        self.in_subagent = true;
        let out = emit(self);
        self.in_subagent = outer;
        out
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows as plain text pairs, for the copy buffer.
    pub(crate) fn text_rows(&self) -> Vec<(u8, String)> {
        self.rows.iter().map(|(t, s, _)| (*t, s.clone())).collect()
    }

    pub(crate) fn rows(&self) -> &[RowText] {
        &self.rows
    }

    pub(crate) fn callids(&self) -> &[Option<String>] {
        &self.callids
    }

    pub(crate) fn fold_keys(&self) -> &[Option<String>] {
        &self.fold_keys
    }

    pub(crate) fn expanded_group(&self) -> &[Option<String>] {
        &self.expanded_group
    }

    pub(crate) fn turn_ids(&self) -> &[Option<RowId>] {
        &self.turn_ids
    }

    pub(crate) fn pre_rendered(&self) -> &[Option<Line<'static>>] {
        &self.pre_rendered
    }

    /// The rows this set shows from top for cap rows, for a view that builds
    /// its rows fresh every frame and so has no cached half to join.
    pub(crate) fn slice(&self, top: usize, cap: usize) -> RowSet {
        self.visible(&RowSet::default(), top, cap)
    }

    /// The visible window over this set followed by a live tail. Both halves
    /// are sliced by the same bounds in one traversal, so the result's arrays
    /// stay in step by construction rather than by a length check at the call
    /// site. A virtual scroll: only the rows the viewport shows are cloned,
    /// which matters for the slot cache that grows with the transcript.
    pub(crate) fn visible(&self, live: &RowSet, top: usize, cap: usize) -> RowSet {
        let slots = self.rows.len();
        let total = slots + live.rows.len();
        let end = top.saturating_add(cap).min(total);
        if top >= end {
            return RowSet::default();
        }
        let slots_at = top.min(slots)..end.min(slots);
        let live_at = top.saturating_sub(slots)..end.saturating_sub(slots);
        RowSet {
            rows: window(&self.rows, &live.rows, slots_at.clone(), live_at.clone()),
            callids: window(
                &self.callids,
                &live.callids,
                slots_at.clone(),
                live_at.clone(),
            ),
            fold_keys: window(
                &self.fold_keys,
                &live.fold_keys,
                slots_at.clone(),
                live_at.clone(),
            ),
            expanded_group: window(
                &self.expanded_group,
                &live.expanded_group,
                slots_at.clone(),
                live_at.clone(),
            ),
            turn_ids: window(
                &self.turn_ids,
                &live.turn_ids,
                slots_at.clone(),
                live_at.clone(),
            ),
            row_ids: window(
                &self.row_ids,
                &live.row_ids,
                slots_at.clone(),
                live_at.clone(),
            ),
            pre_rendered: window(&self.pre_rendered, &live.pre_rendered, slots_at, live_at),
            in_subagent: false,
            line_id: None,
        }
    }

    /// The anchor that stands at this screen row, over the rows this set
    /// holds. None when the set holds no named row to stand on.
    ///
    /// A row past the last of them stands in the live tail, whose rows are not
    /// here: the anchor names the last line, past the end of its text by that
    /// many rows.
    pub(crate) fn anchor_at(&self, row: usize) -> Option<ScrollAnchor> {
        let held = self.rows.len();
        let (row, past) = match held.checked_sub(1) {
            None => return None,
            Some(last) if row > last => (last, Some(row - held)),
            Some(_) => (row, None),
        };
        let row_id = self.row_ids.get(row)?.clone()?;
        let (start, end) = self.run_around(row)?;
        // Standing on one of the line's rows counts the text before that row;
        // standing past them counts the line's whole text, plus the rows below
        // it that the line does not hold.
        let offset = match past {
            None => self.run_width(start, row),
            Some(beyond) => self.run_width(start, end) + beyond,
        };
        Some(ScrollAnchor {
            row: row_id,
            char_offset: offset as u32,
        })
    }

    /// The screen row this anchor resolves to, over the rows this set holds.
    /// None when no row here carries the anchor's identity: the line is either
    /// not resident, or inside a group whose rows the group's own row stands
    /// for.
    pub(crate) fn row_of(&self, anchor: &ScrollAnchor) -> Option<usize> {
        let start = self
            .row_ids
            .iter()
            .position(|id| id.as_ref() == Some(&anchor.row))?;
        let (start, end) = self.run_around(start)?;
        let width = self.run_width(start, end);
        let offset = anchor.char_offset as usize;
        if offset < width {
            return Some(self.row_in_run(start, end, offset));
        }
        // Past the end of the line's text. The live tail when this is the last
        // line; the line's own last row otherwise, since a collapsed or
        // re-wrapped line holds fewer rows than the anchor was taken over and
        // must not drift into the next line.
        if end == self.row_ids.len() {
            Some(end + (offset - width))
        } else {
            Some(end - 1)
        }
    }

    /// The run of rows around this one that share its identity.
    fn run_around(&self, row: usize) -> Option<(usize, usize)> {
        let id = self.row_ids.get(row)?.as_ref()?;
        let mut start = row;
        while start > 0 && self.row_ids[start - 1].as_ref() == Some(id) {
            start -= 1;
        }
        let mut end = row + 1;
        while end < self.row_ids.len() && self.row_ids[end].as_ref() == Some(id) {
            end += 1;
        }
        Some((start, end))
    }

    /// The scalar width of the rows in this run up to, and including the row
    /// break after, each of them. An offset inside the run lands by comparing
    /// it against these cumulated widths.
    fn run_width(&self, start: usize, end: usize) -> usize {
        (start..end)
            .map(|r| self.rows[r].1.chars().count() + 1)
            .sum()
    }

    /// The row an offset lands on inside one run.
    fn row_in_run(&self, start: usize, end: usize, offset: usize) -> usize {
        let mut acc = 0;
        for row in start..end {
            acc += self.rows[row].1.chars().count() + 1;
            if offset < acc {
                return row;
            }
        }
        end - 1
    }
}

/// Concatenate one window of the cached half with one window of the live half.
fn window<T: Clone>(
    slots: &[T],
    live: &[T],
    slots_at: Range<usize>,
    live_at: Range<usize>,
) -> Vec<T> {
    let mut out: Vec<T> = slots[slots_at].to_vec();
    out.extend(live[live_at].iter().cloned());
    out
}
