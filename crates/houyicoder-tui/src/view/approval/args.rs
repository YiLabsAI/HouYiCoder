//! What the approval card shows about the call's arguments, and the rows it
//! may use for them. A card is the one view the user must be able to answer
//! on, so the arguments block is bounded and gives its room back to the
//! question and the options when it cannot hold everything.

use ratatui::text::Line;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::Span,
    widgets::Paragraph,
};
use serde_json::Value;

use crate::records::Approval;

use crate::view::line_wrap::{wrap_line, wrap_styled_line};

/// The most rows the arguments block may take. A long command or a large edit
/// diff used to grow the card without limit, which pushed the question and the
/// options off the screen.
pub(super) const MAX_APPROVAL_ARGS_ROWS: u16 = 12;

/// The rows the arguments block may take in a terminal of the given height:
/// the cap above, or a third of the terminal when that is smaller, so a short
/// window keeps the question and the options on screen. The floor of one row
/// stands in front of the third on a terminal too short to have one, because
/// a budget of zero would hide the marker along with the rows it names.
pub(super) fn row_cap(terminal_h: u16) -> u16 {
    MAX_APPROVAL_ARGS_ROWS.min(terminal_h / 3).max(1)
}

/// The rows the block draws at this width, within the given row budget. Zero
/// when the card has no room left for it.
pub(super) fn height(a: &Approval, width: u16, budget: u16) -> u16 {
    if budget == 0 {
        return 0;
    }
    folded_rows(a, width, budget).len().max(1) as u16
}

/// Render the block into its slot. For edit/multiedit it is a colored
/// old-to-new diff preview; other tools show the extracted command or the raw
/// args, and an entitlement ask renders its own detail lines. The rows arrive
/// already wrapped and bounded, so no second wrap can draw rows the height
/// reserved for the block did not count.
pub(super) fn render(f: &mut Frame, a: &Approval, slot: Rect) {
    if slot.height == 0 {
        return;
    }
    let rows = folded_rows(a, slot.width, slot.height);
    f.render_widget(
        Paragraph::new(rows).style(Style::new().fg(Color::White)),
        slot,
    );
}

/// The block cut down to the rows the budget allows. Every row is already
/// wrapped to the card width, so what the renderer draws is exactly what the
/// height reserved for it counted.
fn folded_rows(a: &Approval, width: u16, budget: u16) -> Vec<Line<'static>> {
    fold_rows(args_rows(a, width), budget, a.expanded, width)
}

fn args_rows(a: &Approval, width: u16) -> Vec<Line<'static>> {
    let parsed = serde_json::from_str::<Value>(&a.args).ok();
    if a.is_entitlement() {
        return plain_rows(&entitlement_detail(parsed.as_ref()), width);
    }
    if let Some(lines) = parsed
        .as_ref()
        .and_then(|value| diff_preview(&a.tool, value))
    {
        return lines
            .into_iter()
            .flat_map(|line| wrap_styled_line(line, width as usize))
            .collect();
    }
    command_rows(&a.tool, parsed.as_ref(), &a.args, width)
}

/// Wrap each logical line of an unstyled block to the card width, so every
/// resulting row is drawn whole.
fn plain_rows(lines: &[Line<'_>], width: u16) -> Vec<Line<'static>> {
    lines
        .iter()
        .flat_map(|line| {
            wrap_line(&line.to_string(), width as usize)
                .into_iter()
                .map(Line::from)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn command_rows(tool: &str, value: Option<&Value>, raw: &str, width: u16) -> Vec<Line<'static>> {
    let full = command(tool, value, raw);
    let available = width.saturating_sub(3) as usize;
    full.split('\n')
        .flat_map(|logical| wrap_line(logical, available))
        .map(|row| Line::from(format!("   {row}")))
        .collect()
}

/// Keep the head and the tail of a block that outgrew its budget, with a dim
/// row naming how many rows were released between them. The head shows what
/// the call starts with; the tail keeps the end, where a pipeline names what
/// it does with the output and an edit shows its final lines, so the tail
/// takes the odd row of an uneven budget. A budget of one has room for the
/// marker alone; any larger budget keeps at least the first row, so the block
/// never opens on the cut alone. The marker names the key that toggles the
/// released rows, in whichever direction the next press goes.
pub(super) fn fold_rows(
    rows: Vec<Line<'static>>,
    budget: u16,
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let budget = usize::from(budget.max(1));
    if rows.len() <= budget {
        return rows;
    }
    if budget == 1 {
        return vec![fold_marker(rows.len(), expanded, width)];
    }
    let room = budget - 1;
    let head = (room / 2).max(1);
    let tail = room - head;
    let hidden = rows.len() - head - tail;
    let mut kept: Vec<Line<'static>> = Vec::with_capacity(budget);
    kept.extend(rows[..head].iter().cloned());
    kept.push(fold_marker(hidden, expanded, width));
    kept.extend(rows[rows.len() - tail..].iter().cloned());
    kept
}

/// The dim row that stands where the released rows were. The key offer is the
/// first part to go when the card is too narrow to hold it: the count is what
/// tells the user rows are missing, so the count outlives the offer.
fn fold_marker(hidden: usize, expanded: bool, width: u16) -> Line<'static> {
    let offer = if expanded {
        " \u{2014} ctrl+o to collapse"
    } else {
        " \u{2014} ctrl+o to expand"
    };
    let candidates = [
        format!("   \u{2026} ({hidden} lines hidden{offer}) \u{2026}"),
        format!("   \u{2026} ({hidden} lines hidden) \u{2026}"),
        format!("   \u{2026} {hidden} hidden \u{2026}"),
    ];
    let text = candidates
        .into_iter()
        .find(|row| row.chars().count() <= usize::from(width))
        .unwrap_or_else(|| format!("   {hidden} \u{2026}"));
    Line::from(Span::styled(text, Style::new().fg(Color::DarkGray)))
}

/// The skill an entitlement ask names, when its input carries one.
pub(super) fn skill(args: &str) -> Option<String> {
    let value: Value = serde_json::from_str(args).ok()?;
    value.get("skill").and_then(Value::as_str).map(String::from)
}

/// Detail lines for the entitlement card: the skill that was blocked, the
/// command that triggered the denial, and each service the deny-log scan
/// found. The command is shown so the user can judge whether the request is
/// legitimate — a service name alone does not tell the user what the skill was
/// trying to do, and the command leads the services because it is what the
/// user judges by when the block clips the lower lines.
pub(super) fn entitlement_detail(parsed: Option<&Value>) -> Vec<Line<'static>> {
    let skill = parsed
        .and_then(|v| v.get("skill"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let origin = parsed
        .and_then(|v| v.get("origin"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let mut lines = vec![Line::from(format!(
        " Skill {skill} ({origin}) was blocked from"
    ))];
    if let Some(cmd) = parsed.and_then(|v| v.get("command"))
        && let Some(cmd_str) = cmd.as_str()
        && !cmd_str.is_empty()
    {
        let display = truncate_tail(cmd_str, 60);
        lines.push(Line::from(format!(" Command: {display}")));
    }
    if let Some(services) = parsed.and_then(|v| v.get("services"))
        && let Some(arr) = services.as_array()
    {
        for s in arr {
            if let Some(name) = s.as_str() {
                lines.push(Line::from(format!(" {name}")));
            }
        }
    }
    lines
}

/// Extract the human-readable command from the tool args. For bash, the
/// command field holds the shell string; for other tools, fall back to the raw
/// args JSON. Returns the raw string when JSON parsing fails. The full command
/// is preserved so the renderer can wrap it to the terminal width.
pub(super) fn command(tool: &str, value: Option<&Value>, raw: &str) -> String {
    let field = match tool.to_ascii_lowercase().as_str() {
        "read" | "write" | "edit" | "multiedit" | "patch" | "str_replace" => "path",
        _ => "command",
    };
    value
        .and_then(|v| v.get(field))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| value.map(Value::to_string))
        .unwrap_or_else(|| raw.to_string())
}

/// Truncate a string to at most max bytes of the tail, prefixing an ellipsis.
/// Keeps the tail (the end of a path or command is the discriminating part)
/// and drops the head. floor_char_boundary prevents splitting a multi-byte
/// character at the byte offset.
fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.floor_char_boundary(s.len() - max);
    format!("…{}", &s[start..])
}

/// Build a colored old→new diff preview for edit/multiedit. Each old line is
/// red (-), each new line green (+). Returns None for other tools or malformed
/// input (caller falls back to raw args). Multi-line strings split on \n.
pub(super) fn diff_preview(tool: &str, input: &Value) -> Option<Vec<Line<'static>>> {
    let red = Style::new().fg(Color::Rgb(255, 107, 128));
    let green = Style::new().fg(Color::Rgb(78, 186, 101));
    let dim = Style::new().fg(Color::DarkGray);
    let pairs: Vec<(&str, &str)> = match tool {
        "edit" => {
            let old = input.get("old_string")?.as_str()?;
            let new = input.get("new_string")?.as_str()?;
            vec![(old, new)]
        }
        "multiedit" => input
            .get("edits")?
            .as_array()?
            .iter()
            .filter_map(|e| {
                let old = e.get("old_string")?.as_str()?;
                let new = e.get("new_string")?.as_str()?;
                Some((old, new))
            })
            .collect(),
        _ => return None,
    };
    if pairs.is_empty() {
        return None;
    }
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, (old, new)) in pairs.iter().enumerate() {
        if pairs.len() > 1 {
            lines.push(Line::from(Span::styled(format!("edit {}:", i + 1), dim)));
        }
        for l in old.split('\n') {
            lines.push(Line::from(vec![
                Span::styled("-", red),
                Span::raw(l.to_string()),
            ]));
        }
        for l in new.split('\n') {
            lines.push(Line::from(vec![
                Span::styled("+", green),
                Span::raw(l.to_string()),
            ]));
        }
    }
    Some(lines)
}

#[cfg(test)]
mod tests;
