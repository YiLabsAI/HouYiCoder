//! Row identity for a selection end.
//!
//! A content row is an index into one row set, and a frame that adds or
//! removes rows above a selection moves every index below it while the text
//! stays put. Each end therefore records the line it stands in and the row it
//! stands on within that line, and every read resolves the end through that
//! name against the rows and names of the frame it draws on.

use super::{Selection, Span};
use crate::transcript::row_id::RowId;

/// A point inside one transcript line: the line's identity and which of its
/// rows the point stands on. A row number alone names the text only until the
/// set is rebuilt; a frame that adds or removes rows above the point moves
/// every number below it while the text stays put.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinePoint {
    line: RowId,
    row_in_line: usize,
}

impl Selection {
    /// Record the line each end stands on, from the rows a view shows. A
    /// gesture calls this after it moves an end; every later read resolves the
    /// ends through these names, so the range stays on its text when rows
    /// above it appear or vanish.
    pub fn bind(&mut self, ids: &[Option<RowId>]) {
        self.anchor_point = self.anchor.and_then(|(_, row)| point_at(ids, row));
        self.focus_point = self.focus.and_then(|(_, row)| point_at(ids, row));
        self.span_points = self
            .span_origin
            .and_then(|span| Some((point_at(ids, span.lo.1)?, point_at(ids, span.hi.1)?)));
    }

    /// This selection with every recorded row moved to where its line stands
    /// in the set named by ids. An end whose line is no longer in the set
    /// keeps the row it was recorded on; that row may then hold other text,
    /// which is the one case this mapping cannot correct for. The
    /// multi-click span moves the same way, so a drag that extends it after
    /// the rows above changed still grows from the word or line the reader
    /// picked.
    pub fn resolved(&self, ids: &[Option<RowId>]) -> Selection {
        let mut out = self.clone();
        out.anchor = self
            .anchor
            .map(|(col, row)| (col, row_of_point(ids, self.anchor_point.as_ref(), row)));
        out.focus = self
            .focus
            .map(|(col, row)| (col, row_of_point(ids, self.focus_point.as_ref(), row)));
        if let Some(span) = self.span_origin {
            let lo_point = self.span_points.as_ref().map(|(lo, _)| lo);
            let hi_point = self.span_points.as_ref().map(|(_, hi)| hi);
            out.span_origin = Some(Span {
                lo: (span.lo.0, row_of_point(ids, lo_point, span.lo.1)),
                hi: (span.hi.0, row_of_point(ids, hi_point, span.hi.1)),
                kind: span.kind,
            });
        }
        out
    }
}

/// The content row the anchor stands on in the set named by ids. Every reader
/// outside a gesture resolves the anchor here, so a key or click acts on the
/// row that now holds the clicked text rather than on a number the last frame
/// rebuilt underneath it.
pub(crate) fn anchor_row(selection: &Selection, ids: &[Option<RowId>]) -> Option<usize> {
    selection
        .anchor
        .map(|(_, row)| row_of_point(ids, selection.anchor_point.as_ref(), row))
}

/// The point this row stands at: the identity on the row plus its position in
/// that line's run of rows. None when the row carries no identity: the live
/// tail is built for the frame rather than read from the stored lines, so a
/// selection starting there keeps raw rows and can still drift on reflow.
fn point_at(ids: &[Option<RowId>], row: usize) -> Option<LinePoint> {
    let line = ids.get(row)?.clone()?;
    let mut start = row;
    while start > 0 && ids[start - 1].as_ref() == Some(&line) {
        start -= 1;
    }
    Some(LinePoint {
        line,
        row_in_line: row - start,
    })
}

/// The row a point stands on in the set named by ids. The row it was recorded
/// on when the line is not in this set, or holds fewer rows than the point
/// stood past: text that shrank must not drift into the next line.
fn row_of_point(ids: &[Option<RowId>], point: Option<&LinePoint>, row: usize) -> usize {
    let Some(point) = point else {
        return row;
    };
    let Some(start) = ids.iter().position(|id| id.as_ref() == Some(&point.line)) else {
        return row;
    };
    let mut end = start + 1;
    while end < ids.len() && ids[end].as_ref() == Some(&point.line) {
        end += 1;
    }
    start + point.row_in_line.min(end - start - 1)
}

#[cfg(test)]
mod tests;
