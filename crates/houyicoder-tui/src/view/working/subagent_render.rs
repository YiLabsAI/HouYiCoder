//! Inline render of a sub-agent delegation as a fold-group in the parent
//! flow. Split from working_transcript so the row builder stays under the
//! size gate.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

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
    // A delegation nested inside an expanded one is shown, not operated: it
    // carries no hint and no fold key, so the block the user opened stays the
    // only thing their next Ctrl+O or click acts on.
    let nested = sink.in_subagent();
    let hint = match (nested, expanded) {
        (true, _) => String::new(),
        (false, true) => "  (ctrl+o to collapse)".to_string(),
        (false, false) => "  (ctrl+o to expand)".to_string(),
    };
    let head = format!("\u{23bf} {subagent_type}: {summary}{hint}");
    // Plain tag so the head stays drag-selectable, plus the child session id
    // as its fold key: the mouse-down fold-key branch runs before selection
    // starts, so a click on the head toggles instead of selecting.
    let fold_key = (!nested).then(|| child_sid.to_string());
    // Expanded, the head belongs to its own block so it shades with it;
    // collapsed, it inherits whatever group encloses it.
    let enclosing = if expanded {
        Some(child_sid.to_string())
    } else {
        grp_key.clone()
    };
    sink.push(
        Row::new(crate::selection::TAG_PLAIN, head.clone())
            .fold_key(fold_key)
            .group(enclosing.clone())
            .pre(Some(head_line(subagent_type, summary, &hint, color))),
    );
    if !expanded {
        return;
    }
    if folded_transcript.is_empty() {
        sink.push(Row::new(SYSTEM, "  child transcript not yet loaded").group(enclosing));
        return;
    }
    sink.within_subagent(|sink| {
        for child in folded_transcript {
            super::working_transcript::push_line_rows(child, Some(child_sid), width, app, sink);
        }
    });
}

/// The head as a styled line: dim throughout, with the agent type in its
/// badge color when it has one. Dim is this transcript's mark of a collapsed
/// block -- every other fold handle carries it, and the head was the one
/// expandable row rendered as ordinary content, which is why it read as a
/// message that happened to mention a keybinding. The type keeps its color so
/// parallel delegations stay distinguishable at a glance.
fn head_line(subagent_type: &str, summary: &str, hint: &str, color: Option<&str>) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let type_style = color
        .and_then(badge_color)
        .map_or(dim, |c| Style::default().fg(c));
    Line::from(vec![
        Span::styled("\u{23bf} ".to_string(), dim),
        Span::styled(subagent_type.to_string(), type_style),
        Span::styled(format!(": {summary}{hint}"), dim),
    ])
}
