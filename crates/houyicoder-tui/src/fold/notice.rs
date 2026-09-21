//! The memory-change notice block: the rows it renders to, shared by the
//! renderer and the fold-aware count so a summary that outgrows the pane wraps
//! into the rows it reserves.

use crate::fold::DisplaySlot;
use crate::records::TranscriptLine;
use crate::toggle_hint::ToggleHint;
use crate::view::line_wrap::wrap_line;

/// The terminal rows a memory-change notice renders to at a width: the
/// summary row trailing the toggle that matches the state it is in, then,
/// when expanded, one row per changed key. One entry per rendered row, so the
/// count and the renderer walk the same list and agree at any width.
pub(crate) fn notice_lines(text: &str, expanded: bool, width: usize) -> Vec<String> {
    let action = if expanded {
        ToggleHint::Collapse
    } else {
        ToggleHint::Expand
    };
    let mut logical = text.split('\n');
    let summary = logical.next().unwrap_or("");
    let mut rows = wrap_line(&format!("✻ {summary}{}", action.suffix()), width);
    if expanded {
        for line in logical {
            rows.extend(wrap_line(line, width));
        }
    }
    rows
}

/// The rows a notice slot occupies. The slot carries the expanded state (the
/// fold key is an occurrence ordinal, not a property of the line), so the
/// caller passes the slot it is counting.
pub(crate) fn notice_slot_rows(slot: &DisplaySlot, line: &TranscriptLine, width: u16) -> usize {
    match line {
        TranscriptLine::System(text) => {
            let expanded = matches!(slot, DisplaySlot::NoticeExpanded { .. });
            notice_lines(text, expanded, width as usize).len()
        }
        // Only notice slots call this; a line that is not a notice never
        // reaches here, and one row is the safe count if it ever does.
        _ => 1,
    }
}
