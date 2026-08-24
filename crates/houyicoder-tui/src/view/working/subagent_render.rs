//! Inline render of a sub-agent delegation as a fold-group in the parent
//! flow. Split from working_transcript so the row builder stays under the
//! size gate.

use ratatui::text::Line;

use crate::records::TranscriptLine;
use crate::state::App;

/// Render a Subagent delegation as an inline fold-group in the parent flow.
/// Collapsed shows the subagent type + summary + an expand hint; expanded
/// shows the collapse hint + the child transcript rows once loaded. The
/// expand state is keyed by child_sid so it survives the per-batch
/// transcript rebuild. The parent message list is never swapped out.
#[expect(clippy::too_many_arguments, reason = "row builder params")]
pub(crate) fn push_subagent_rows(
    child_sid: &str,
    subagent_type: &str,
    summary: &str,
    folded_transcript: &[TranscriptLine],
    grp: Option<&str>,
    width: u16,
    app: &App,
    rows: &mut Vec<(u8, String, Option<crate::records::ToolOutcome>)>,
    row_callids: &mut Vec<Option<String>>,
    fold_keys: &mut Vec<Option<String>>,
    expanded_group: &mut Vec<Option<String>>,
    turn_ids: &mut Vec<Option<String>>,
    pre_rendered: &mut Vec<Option<Line<'static>>>,
) {
    const PLAIN: u8 = crate::selection::TAG_PLAIN;
    const SYSTEM: u8 = crate::selection::TAG_SYSTEM;
    let grp_key: Option<String> = grp.map(|g| g.to_string());
    let expanded = app.expanded_subagents.contains(child_sid);
    let hint = if expanded {
        "(ctrl+o to collapse)"
    } else {
        "(ctrl+o to expand)"
    };
    let head = format!("\u{23bf} {subagent_type}: {summary}  {hint}");
    rows.push((PLAIN, head, None));
    row_callids.push(None);
    fold_keys.push(None);
    expanded_group.push(grp_key.clone());
    turn_ids.push(None);
    pre_rendered.push(None);
    if expanded {
        if folded_transcript.is_empty() {
            rows.push((SYSTEM, "  child transcript loads on fetch".into(), None));
            row_callids.push(None);
            fold_keys.push(None);
            expanded_group.push(grp_key);
            turn_ids.push(None);
            pre_rendered.push(None);
        } else {
            for child in folded_transcript {
                super::working_transcript::push_line_rows(
                    child,
                    grp,
                    width,
                    app,
                    rows,
                    row_callids,
                    fold_keys,
                    expanded_group,
                    turn_ids,
                    pre_rendered,
                );
            }
        }
    }
}
