//! Folded and expanded rows for a delegation in the parent transcript.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::transcript::push_line_rows;
use crate::rows::row_set::{Row, RowSet};
use crate::selection::TAG_PLAIN;
use crate::selection::TAG_SYSTEM;
use crate::state::App;
use crate::toggle_hint::ToggleHint;
use crate::transcript::records::{SubagentFinished, SubagentOutcome, TranscriptLine};
use crate::view::badge_color;
use crate::view::line_wrap::truncate_width;
use crate::view::{ERROR_RED, SUCCESS_GREEN};

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
pub(crate) fn push_delegation_rows(
    d: &Delegation<'_>,
    grp: Option<&str>,
    width: u16,
    app: &App,
    sink: &mut RowSet,
) {
    const SYSTEM: u8 = TAG_SYSTEM;
    let Delegation {
        child_sid,
        subagent_type,
        summary,
        folded_transcript,
        color,
    } = *d;
    let grp_key: Option<String> = grp.map(|g| g.to_string());
    let expanded = app
        .ui
        .transcript_view
        .expanded
        .subagents
        .contains(child_sid);
    // A delegation nested inside an expanded one is shown, not operated: it
    // carries no hint and no fold key, so the block the user opened stays the
    // only thing their next Ctrl+O or click acts on.
    let nested = sink.in_subagent();
    let hint = match (nested, expanded) {
        (true, _) => String::new(),
        (false, true) => ToggleHint::Collapse.suffix().to_string(),
        (false, false) => ToggleHint::Expand.suffix().to_string(),
    };
    // The hint is the row's only affordance, so it is reserved before the
    // summary takes its share: a summary long enough to fill the row used to
    // push the closing half of the chord past the right edge, leaving the
    // reader with a truncated "(ctrl+" and no way to know the row folds.
    let (summary, hint) = fit_head(subagent_type, summary, &hint, width);
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
        Row::new(TAG_PLAIN, head.clone())
            .fold_key(fold_key)
            .group(enclosing.clone())
            .pre(Some(head_line(subagent_type, &summary, &hint, color))),
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
            push_line_rows(child, Some(child_sid), width, app, sink);
        }
    });
}

/// The finished-child trace as one row: the status mark carries the ending's
/// color, and the rest reads as ordinary text so the row stays selectable and
/// searchable. Green is the same success color the tool-call marks use, red
/// the same error color; interrupted takes yellow, and an ending this build
/// cannot name stays neutral rather than borrowing a verdict it never saw.
/// The row also carries the child's session id, so a clean click opens the
/// same child the drill-in key opens while a drag still copies the text.
pub(crate) fn push_finished_rows(child: &SubagentFinished, width: u16, sink: &mut RowSet) {
    let (name, suffix) = fit_finished(child, width);
    let rest = match &name {
        Some(name) => format!(" Agent \"{name}{suffix}"),
        None => format!(" Agent {suffix}"),
    };
    sink.push(
        Row::new(TAG_PLAIN, format!("\u{25cf}{rest}"))
            .trace_child(Some(child.child_sid.clone()))
            .pre(Some(Line::from(vec![
                Span::styled("\u{25cf}".to_string(), finished_mark_style(child.outcome)),
                Span::raw(rest),
            ]))),
    );
}

/// The mark and the opening quote every trace row with a name draws before
/// it. Every part but the name is measured before the name takes its share
/// of the row.
const FINISHED_PREFIX: &str = "\u{25cf} Agent \"";

/// The same lead without the quote pair, for a row too narrow to draw any
/// name: empty quotes would read as a delegation with a blank task rather
/// than one whose name had to be dropped.
const UNNAMED_PREFIX: &str = "\u{25cf} Agent ";

fn finished_mark_style(outcome: SubagentOutcome) -> Style {
    let color = match outcome {
        SubagentOutcome::Finished | SubagentOutcome::TurnLimit | SubagentOutcome::Budget => {
            SUCCESS_GREEN
        }
        SubagentOutcome::Failed => ERROR_RED,
        SubagentOutcome::Interrupted => Color::Yellow,
        SubagentOutcome::Unknown => Color::DarkGray,
    };
    Style::default().fg(color)
}

/// Fit the trace to one row, in the order its parts matter.
///
/// The ending and the measured figures are whole facts and the name is a
/// label, so they keep their columns first and the name takes what is left.
/// When nothing is left, the row drops the quote pair and reads as the
/// ending alone: a clipped ending reports an outcome nobody can read, while
/// an empty pair of quotes reads as a delegation with a blank task. Only a
/// row too narrow for the ending itself shortens the ending.
///
/// Every measurement is display columns, not characters: a name written in
/// wide characters takes two columns each, and counting characters would
/// hand the row a line twice its width.
fn fit_finished(child: &SubagentFinished, width: u16) -> (Option<String>, String) {
    let ending = child.ending();
    let duration = child.duration_text().unwrap_or_default();
    let tools = child.tools_text();
    let budget = (width as usize).saturating_sub(UnicodeWidthStr::width(FINISHED_PREFIX));
    let suffix = [
        format!("{ending}{duration}{tools}"),
        format!("{ending}{duration}"),
    ]
    .into_iter()
    .find(|candidate| UnicodeWidthStr::width(candidate.as_str()) <= budget)
    .unwrap_or_else(|| ending.clone());
    let room = budget.saturating_sub(UnicodeWidthStr::width(suffix.as_str()));
    if room > 0 {
        return (Some(truncate_width(&child.name(), room)), suffix);
    }
    fit_unnamed(child, width)
}

/// The same fit without a name. The quote pair gives its two columns back to
/// the ending and its figures, so more of them survive on a narrow row.
fn fit_unnamed(child: &SubagentFinished, width: u16) -> (Option<String>, String) {
    let budget = (width as usize).saturating_sub(UnicodeWidthStr::width(UNNAMED_PREFIX));
    let label = child.outcome.label();
    let duration = child.duration_text().unwrap_or_default();
    let tools = child.tools_text();
    let suffix = [
        format!("{label}{duration}{tools}"),
        format!("{label}{duration}"),
        label.to_string(),
    ]
    .into_iter()
    .find(|candidate| UnicodeWidthStr::width(candidate.as_str()) <= budget)
    .unwrap_or_else(|| label.to_string());
    (None, truncate_width(&suffix, budget))
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

/// Fit the head to one row, in the order the parts matter.
///
/// The hint names the row's only gesture, so it keeps its width first and the
/// summary gives way: a reader who sees the opening of a key they cannot
/// finish reading has learned nothing, while a summary cut short still names
/// the row. The hint is dropped whole only when the row cannot hold the marker,
/// the type and the hint together, where drawing it would mean clipping it.
///
/// Every measurement here is display columns, not characters. A summary
/// written in wide characters occupies two columns each, so a character
/// count undercounts by half and hands the row a line twice its width,
/// which clips the chord just as the unbounded summary did.
fn fit_head(subagent_type: &str, summary: &str, hint: &str, width: u16) -> (String, String) {
    // The marker, the type, and the separator between the type and the
    // summary: all three are drawn whatever the summary does.
    let fixed = DISPLAY_WIDTH_OF_MARKER + UnicodeWidthStr::width(subagent_type) + 2;
    let width = width as usize;
    let hint_width = UnicodeWidthStr::width(hint);
    let (hint, room) = if fixed + hint_width <= width {
        (hint.to_string(), width - fixed - hint_width)
    } else {
        (String::new(), width.saturating_sub(fixed))
    };
    (truncate_width(summary, room), hint)
}

/// Display columns the marker and its trailing space occupy: one each.
const DISPLAY_WIDTH_OF_MARKER: usize = 2;
