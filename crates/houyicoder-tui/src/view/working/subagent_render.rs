//! Inline render of a sub-agent delegation as a fold-group in the parent
//! flow. Split from working_transcript so the row builder stays under the
//! size gate.

use ratatui::style::Style;
use ratatui::text::Line;

use super::row_sink::{Row, RowSink};
use crate::records::TranscriptLine;
use crate::state::App;
use crate::view::badge_color;

/// One delegation's render inputs, borrowed from its transcript line. Grouped
/// because they describe one thing; as loose parameters they said nothing
/// about belonging to the same delegation.
pub(crate) struct Delegation<'a> {
    pub child_sid: &'a str,
    pub subagent_type: &'a str,
    pub summary: &'a str,
    pub folded_transcript: &'a [TranscriptLine],
    pub color: Option<&'a str>,
}

/// Render a Subagent delegation as an inline fold-group in the parent flow.
/// Collapsed shows the subagent type + summary + an expand hint; expanded
/// shows the collapse hint + the child transcript rows once loaded. The
/// expand state is keyed by child_sid so it survives the per-batch
/// transcript rebuild. The parent message list is never swapped out. The
/// badge color, when set, tints the summary header so multiple delegations
/// are distinguishable at a glance.
pub(crate) fn push_subagent_rows(
    d: &Delegation<'_>,
    grp: Option<&str>,
    width: u16,
    app: &App,
    sink: &mut RowSink,
) {
    const SYSTEM: u8 = crate::selection::TAG_SYSTEM;
    let Delegation {
        child_sid,
        subagent_type,
        summary,
        folded_transcript,
        color,
    } = *d;
    let grp_key: Option<String> = grp.map(|g| g.to_string());
    let expanded = app.expanded_subagents.contains(child_sid);
    let hint = if expanded {
        "(ctrl+o to collapse)"
    } else {
        "(ctrl+o to expand)"
    };
    let head = format!("\u{23bf} {subagent_type}: {summary}  {hint}");
    // Plain tag so the head stays drag-selectable, plus the child session id
    // as its fold key: the mouse-down fold-key branch runs before selection
    // starts, so a click on the head toggles instead of selecting.
    let styled = color
        .and_then(badge_color)
        .map(|c| Line::from(head.clone()).style(Style::default().fg(c)));
    sink.push(
        Row::new(crate::selection::TAG_PLAIN, head)
            .fold_key(Some(child_sid.to_string()))
            .group(grp_key.clone())
            .pre(styled),
    );
    if !expanded {
        return;
    }
    if folded_transcript.is_empty() {
        sink.push(Row::new(SYSTEM, "  child transcript not yet loaded").group(grp_key));
        return;
    }
    for child in folded_transcript {
        super::working_transcript::push_line_rows(child, grp, width, app, sink);
    }
}
