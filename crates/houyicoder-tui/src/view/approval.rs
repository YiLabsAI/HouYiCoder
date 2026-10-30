//! Tool-approval prompt rendered inline at the transcript tail. A thin
//! top separator replaces the heavy box border so the card reads as
//! appended content, not a floating modal. Shows the tool header, the
//! command/args, a proceed question, and numbered Yes/No options with a
//! cursor marker on the focused one.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Clear, Paragraph},
};
use serde_json::Value;
use std::time::Instant;

use crate::records::Approval;

use super::line_wrap::{truncate_width, wrap_line};
use super::navigation::key_hint;
use crate::state::App;
use houyicoder_protocol::frontend::permission::AskSource;
use houyicoder_protocol::frontend::run::ApprovalScope;

mod args;

/// Render the approval prompt inline at the transcript tail. A thin
/// horizontal rule at the top replaces the heavy double border; the card
/// reads as inline content, not a modal popup.
///
/// Every row is taken from the slot the working layout reserved, top to
/// bottom, so the rows the card draws never disagree with the rows the layout
/// set aside. The question and the numbered options are reserved first. The
/// optional rows are released in the reverse of their value: the reason
/// first, then the arguments block down to its single marker row, then the key
/// hint, the header, and the separator. A slot too small for the question and
/// all of the options falls back to the focused option alone, because the keys
/// act on that one whether or not the rest of the list fits.
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(a) = app.approval() else {
        return;
    };
    if area.height == 0 {
        return;
    }
    f.render_widget(Clear, area);
    if area.height < essential_rows(a) {
        draw_narrow(f, a, area);
        return;
    }
    let plan = plan_card(a, area.width, area.height, f.area().height);
    let mut rows = CardRows::new(area);

    if plan.separator {
        let sep: String = "─".repeat(area.width as usize);
        f.render_widget(
            Paragraph::new(sep).style(Style::new().fg(Color::DarkGray)),
            rows.take(1),
        );
    }

    // Header: "<Tool> command", prefixed with the child agent type when the
    // ask was routed up from a delegation so the user can tell a child's ask
    // from the parent's own tool call. An entitlement ask renders its own
    // title — it is not a tool call but a deny-log discovery.
    if plan.header {
        let header = if a.is_entitlement() {
            " Sandbox entitlement".to_string()
        } else {
            let title = approval_title(&a.tool);
            match &a.delegation {
                // The marker has to be unmistakable: several children can be
                // running, and a card that reads like the parent's own ask
                // makes the user answer for the wrong agent.
                Some(d) => format!(" \u{29c9} {} (subagent) \u{b7} {title}", d.subagent_type),
                None => format!(" {title}"),
            }
        };
        f.render_widget(
            Paragraph::new(header)
                .style(Style::new().fg(Color::White).add_modifier(Modifier::BOLD)),
            rows.take(1),
        );
    }

    if plan.args > 0 {
        args::render(f, a, rows.take(plan.args));
    }

    if plan.reason > 0 {
        let shown: Vec<Line<'static>> = reason_lines(a, area.width)
            .into_iter()
            .take(usize::from(plan.reason))
            .collect();
        f.render_widget(
            Paragraph::new(shown).style(Style::new().fg(Color::DarkGray)),
            rows.take(plan.reason),
        );
    }

    let question = if a.is_entitlement() {
        format!(
            " Always authorize this service for {}?",
            args::skill(&a.args).unwrap_or_default()
        )
    } else {
        " Do you want to proceed?".to_string()
    };
    f.render_widget(Paragraph::new(question), rows.take(1));

    render_options(f, a, &mut rows);

    if plan.hint {
        // The footer follows the shared key-hint grammar so a user reads the
        // same key names here as in every other pane. The numbered keys are
        // not offered: the arrows already move the selection, and naming two
        // ways to do one thing doubles the row without adding an action.
        let hint = key_hint(&[
            ("Up/Down", "select"),
            ("Enter", "confirm"),
            ("Esc", "cancel"),
        ]);
        f.render_widget(
            Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
            rows.take(1),
        );
    }
}

/// Hands out the rows of the card from the top, so a slot too small for every
/// row clips the last one rather than letting the layout solver spread the
/// deficit over rows the user has to read.
struct CardRows {
    area: Rect,
    next: u16,
}

impl CardRows {
    fn new(area: Rect) -> Self {
        Self { area, next: 0 }
    }

    fn take(&mut self, height: u16) -> Rect {
        let left = self.area.height.saturating_sub(self.next);
        let height = height.min(left);
        let slot = Rect {
            x: self.area.x,
            y: self.area.y + self.next,
            width: self.area.width,
            height,
        };
        self.next += height;
        slot
    }
}

/// The rows the card needs when nothing else constrains it: the essential
/// rows, the chrome, the arguments block at its cap, and the reason, cut down
/// to the room the layout can give. The arguments figure is computed against
/// the same remainder plan_card works from, so the two agree on what the
/// optional rows may claim.
pub(super) fn card_height(a: &Approval, width: u16, room: u16, terminal_h: u16) -> u16 {
    let want_args = args_budget(a, width, room.saturating_sub(essential_rows(a)), terminal_h);
    let want = essential_rows(a) + CHROME_ROWS + want_args + reason_height(a, width);
    want.min(room)
}

/// Which optional rows the card draws in a slot of the given height, and how
/// many rows the arguments block gets. The question and the options are
/// reserved first; everything else competes for what remains and is released
/// in the reverse of its value: the reason, the arguments block down to its
/// single marker row, the key hint, the header, and last the separator. A
/// cramped card therefore loses the context before it loses a choice. A slot
/// shorter than the reserved rows cannot keep that promise, which is why the
/// caller renders the focused option alone instead of coming here.
struct CardPlan {
    separator: bool,
    header: bool,
    args: u16,
    reason: u16,
    hint: bool,
}

/// The rows outside the plan that every card carries: the question and the
/// numbered options it offers.
pub(super) fn essential_rows(a: &Approval) -> u16 {
    if a.two_option_card() { 3 } else { 4 }
}

/// The card as a single row, for a slot too short to hold the question and
/// the numbered list. The focused verdict comes first so a narrow terminal
/// truncates the key hint rather than the choice the keys will act on.
fn draw_narrow(f: &mut Frame, a: &Approval, area: Rect) {
    let line = format!(
        " {} {}. {}  Up/Down to select \u{b7} Enter to confirm \u{b7} Esc to cancel",
        "\u{276f}",
        option_number(a, a.selected),
        option_label(a, a.selected),
    );
    let text = wrapped_rows(&line, area.width)
        .into_iter()
        .next()
        .unwrap_or_else(|| Line::from(line));
    f.render_widget(
        Paragraph::new(text).style(option_style(true)),
        Rect { height: 1, ..area },
    );
}

/// The number a card prints beside an option. The display order is Yes,
/// Yes-don't-ask, No; the selected index keeps its internal mapping (0=Yes,
/// 1=No, 2=Yes-don't-ask), so the two orders meet here.
fn option_number(a: &Approval, selected: usize) -> usize {
    if a.two_option_card() {
        if selected == 1 { 2 } else { 1 }
    } else {
        match selected {
            0 => 1,
            1 => 3,
            _ => 2,
        }
    }
}

/// The label a card prints for one option.
fn option_label(a: &Approval, selected: usize) -> String {
    if a.is_entitlement() && selected == 0 {
        return "Always allow".to_string();
    }
    if a.two_option_card() {
        return if selected == 1 { "No" } else { "Yes" }.to_string();
    }
    match selected {
        0 => yes_label(a),
        2 => format!("Yes, and don't ask again for {}", remember_label(a)),
        _ => "No".to_string(),
    }
}

/// The Yes label, with the auto-approve countdown hint when one is running.
/// The hint names the seconds left so the user can see the card is about to
/// resolve on its own; a touch of any navigation key cancels it.
fn yes_label(a: &Approval) -> String {
    let base = "Yes".to_string();
    if let Some(secs) = a.auto_approve_secs_left(Instant::now()) {
        format!("{base} (auto in {secs}s)")
    } else {
        base
    }
}

/// The rows of chrome the card draws when it has room: the separator, the
/// header, and the key hint.
const CHROME_ROWS: u16 = 3;

fn plan_card(a: &Approval, width: u16, slot: u16, terminal_h: u16) -> CardPlan {
    let left = slot.saturating_sub(essential_rows(a));
    let mut args = args_budget(a, width, left, terminal_h);
    let mut reason = reason_height(a, width);
    let mut chrome = CHROME_ROWS;
    let mut excess = (args + reason + chrome).saturating_sub(left);
    // The reason names why the gate surfaced the call; the arguments name what
    // the call does. The user answers the second, so the first gives way.
    let give_reason = reason.min(excess);
    reason -= give_reason;
    excess -= give_reason;
    // The block keeps one row as long as it asks for any: that row is the
    // marker, which is what tells the user a command is hidden and offers the
    // key that shows it.
    if excess > 0 && args > 1 {
        let give_args = (args - 1).min(excess);
        args -= give_args;
        excess -= give_args;
    }
    let mut hint = true;
    let mut header = true;
    let mut separator = true;
    while excess > 0 && chrome > 0 {
        if hint {
            hint = false;
        } else if header {
            header = false;
        } else {
            separator = false;
        }
        chrome -= 1;
        excess -= 1;
    }
    CardPlan {
        separator,
        header,
        args,
        reason,
        hint,
    }
}

/// The rows the arguments block asks for. Collapsed it stops at the row cap;
/// expanded it takes every row the card has left, which is how the user reads
/// a call the collapsed block released.
fn args_budget(a: &Approval, width: u16, left: u16, terminal_h: u16) -> u16 {
    let room = left.saturating_sub(CHROME_ROWS);
    let budget = if a.expanded {
        room
    } else {
        room.min(args::row_cap(terminal_h))
    };
    args::height(a, width, budget)
}

/// The reason rows: the source label and the gate's detail, wrapped to the
/// card width so the rows counted here are the rows drawn. A reason that
/// arrived without a source renders the detail alone rather than opening on a
/// bare colon. A containment note attached by the fence renders on its own
/// line beneath.
fn reason_lines(a: &Approval, width: u16) -> Vec<Line<'static>> {
    let label = source_label(a);
    let line = if label.is_empty() {
        format!(" {}", a.reason)
    } else {
        format!(" {label}: {}", a.reason)
    };
    let mut rows = wrapped_rows(&line, width);
    if let Some(note) = &a.containment_note {
        rows.extend(wrapped_rows(&format!(" {note}"), width));
    }
    rows
}

fn reason_height(a: &Approval, width: u16) -> u16 {
    reason_lines(a, width).len() as u16
}

fn wrapped_rows(text: &str, width: u16) -> Vec<Line<'static>> {
    text.split('\n')
        .flat_map(|line| wrap_line(line, width as usize))
        .map(Line::from)
        .collect()
}

/// Render the verdict options into their rows. Display order is Yes then
/// Yes-don't-ask then No; the selected index keeps its internal mapping
/// (0=Yes, 1=No, 2=Yes-don't-ask). A two-option card (protected-path or
/// entitlement) hides Yes-don't-ask and renumbers No to 2.
///
/// The remember option names every directory a yes installs, which can be
/// wider than a narrow card. It is clipped with an ellipsis rather than left
/// to the frame edge, so a row that continues still says so.
fn render_options(f: &mut Frame, a: &Approval, rows: &mut CardRows) {
    for selected in option_order(a) {
        let focused = a.selected == selected;
        let marker = if focused { "❯" } else { " " };
        let row = format!(
            " {marker} {}. {}",
            option_number(a, selected),
            option_label(a, selected),
        );
        let suffix = if selected == 2 {
            remember_lifetime(a)
        } else {
            None
        };
        let slot = rows.take(1);
        f.render_widget(
            Paragraph::new(fit_option_row(&row, suffix, slot.width as usize))
                .style(option_style(focused)),
            slot,
        );
    }
}

/// The lifetime the remember option states, spelled as the server spells it
/// on the label. The client renders this tail itself so the clipping below can
/// reserve its width; a card from a server that declared no lifetime has no
/// tail to keep.
fn remember_lifetime(a: &Approval) -> Option<&'static str> {
    match a.remember_scope()? {
        ApprovalScope::Session => Some(" (this session)"),
        ApprovalScope::Workspace => Some(" (this workspace)"),
        ApprovalScope::Once => None,
    }
}

/// Clip an option row to its slot while keeping the declared lifetime whole.
/// The lifetime is the one part of the remember row the user has to read to
/// choose it, and the tail is what clipping drops first; shortening the
/// effect instead leaves both the scope and the fact that the row continues.
fn fit_option_row(row: &str, suffix: Option<&str>, width: usize) -> String {
    let Some(suffix) = suffix.filter(|tail| row.ends_with(tail)) else {
        return truncate_width(row, width);
    };
    let tail_width = suffix.chars().count();
    let Some(head_width) = width.checked_sub(tail_width) else {
        return truncate_width(row, width);
    };
    if head_width == 0 {
        return truncate_width(row, width);
    }
    let head = &row[..row.len() - suffix.len()];
    format!("{}{suffix}", truncate_width(head, head_width))
}

/// The option indexes in the order the card prints them. The entrance
/// sequence is Yes, then Yes-don't-ask, then No, whatever number the choice
/// keeps in the record.
fn option_order(a: &Approval) -> Vec<usize> {
    if a.two_option_card() {
        vec![0, 1]
    } else {
        vec![0, 2, 1]
    }
}

fn option_style(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::White)
    }
}

/// The scope text for the remember option. The server states it, because only
/// the server knows what a yes-always installs; a card built without a server
/// option set falls back to the tool's first word, which is the shape older
/// engines understood.
fn remember_label(a: &Approval) -> String {
    if let Some(label) = a.remember_name() {
        return label.to_string();
    }
    match a.tool.to_ascii_lowercase().as_str() {
        "bash" | "sh" | "exec" | "shell" => {
            let val = serde_json::from_str::<Value>(&a.args).ok();
            let cmd = args::command(&a.tool, val.as_ref(), &a.args);
            cmd.split_whitespace()
                .next()
                .map(|t| format!("{t} *"))
                .unwrap_or_else(|| a.tool.clone())
        }
        _ => a.tool.clone(),
    }
}

/// A short label for the ask source, rendered as a prefix on the reason
/// line so the user reads which class of check escalated the call. Empty
/// when the ask carries no source, which is the generic prompt.
fn source_label(a: &Approval) -> &'static str {
    if a.is_entitlement() {
        return "Deny-log discovery";
    }
    match a.source {
        Some(AskSource::SystemSafety) => "Protected path",
        Some(AskSource::Detection) => "Detection",
        Some(AskSource::UserRule) => "Rule",
        Some(AskSource::ToolNative) => "Tool",
        // The unknown fallback (a future engine source) has no label.
        _ => "",
    }
}

fn approval_title(tool: &str) -> String {
    let noun = match tool.to_ascii_lowercase().as_str() {
        "read" => "Read file",
        "write" => "Write file",
        "edit" | "multiedit" | "patch" | "str_replace" => "Edit file",
        _ => return format!("{} command", cap_first(tool)),
    };
    noun.to_string()
}

/// Capitalize the first character of a tool name ("bash" -> "Bash").
fn cap_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests;
