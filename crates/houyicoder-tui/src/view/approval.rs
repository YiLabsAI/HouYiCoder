//! Tool-approval prompt rendered inline at the transcript tail. A thin
//! top separator replaces the heavy box border so the card reads as
//! appended content, not a floating modal. Shows the tool header, the
//! command/args, a proceed question, and numbered Yes/No options with a
//! cursor marker on the focused one.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};
use serde_json::Value;

use crate::state::App;

/// Render the approval prompt inline at the transcript tail. A thin
/// horizontal rule at the top replaces the heavy double border; the card
/// reads as inline content, not a modal popup.
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(a) = app.approval.as_ref() else {
        return;
    };
    f.render_widget(Clear, area);
    let chunks = split_card(area, a.two_option_card());

    // Top separator: full-width thin horizontal rule.
    let sep: String = "─".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(sep).style(Style::new().fg(Color::DarkGray)),
        chunks[0],
    );

    // Header: "<Tool> command", prefixed with the child agent type when the
    // ask was routed up from a delegation so the user can tell a child's ask
    // from the parent's own tool call. An entitlement ask renders its own
    // title — it is not a tool call but a deny-log discovery.
    let header = if a.is_entitlement() {
        " Sandbox entitlement".to_string()
    } else {
        match &a.delegation {
            Some(d) => format!(" {} · {} command", d.subagent_type, cap_first(&a.tool)),
            None => format!(" {} command", cap_first(&a.tool)),
        }
    };
    f.render_widget(
        Paragraph::new(header).style(Style::new().fg(Color::White).add_modifier(Modifier::BOLD)),
        chunks[1],
    );

    render_args(f, a, chunks[2]);

    // Reason (dim) — why the gate surfaced this call. The structured
    // AskReason detail the gate produced, prefixed with a short source label
    // (Protected path / Detection / Rule / Tool) so the user reads which
    // class of check escalated, not just one sentence. A generic prompt
    // renders when the composition root could not reconstruct a reason. When
    // the containment layer attached a note, it renders on its own line
    // beneath.
    let mut reason_text = vec![Line::from(format!(" {}: {}", source_label(a), a.reason))];
    if let Some(note) = &a.containment_note {
        reason_text.push(Line::from(format!(" {}", note)));
    }
    f.render_widget(
        Paragraph::new(reason_text).style(Style::new().fg(Color::DarkGray)),
        chunks[3],
    );

    // Question
    let question = if a.is_entitlement() {
        format!(
            " Always authorize this service for {}?",
            entitlement_skill(&a.args).unwrap_or_default()
        )
    } else {
        " Do you want to proceed?".to_string()
    };
    f.render_widget(Paragraph::new(question), chunks[4]);

    render_options(f, a, &chunks);

    let hint = if a.two_option_card() {
        " ↑↓ navigate · 1/2 select · Enter confirm · Esc cancel"
    } else {
        " ↑↓ navigate · 1/2/3 select · Enter confirm · Esc cancel"
    };
    let hint_idx = if a.two_option_card() { 7 } else { 8 };
    f.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        chunks[hint_idx],
    );
}

/// Split the card area into vertical slots. A two-option card drops the
/// third option slot entirely; a three-option card keeps it. No fixed gap
/// rows — widgets carry their own visual separation. Reason is Min(1) so
/// it grows only when a containment note is present.
fn split_card(area: Rect, two_option: bool) -> std::rc::Rc<[Rect]> {
    let constraints = if two_option {
        vec![
            Constraint::Length(1), // [0] separator
            Constraint::Length(1), // [1] header
            Constraint::Min(1),    // [2] args
            Constraint::Min(1),    // [3] reason
            Constraint::Length(1), // [4] question
            Constraint::Length(1), // [5] option 1
            Constraint::Length(1), // [6] option 2
            Constraint::Length(1), // [7] hint
        ]
    } else {
        vec![
            Constraint::Length(1), // [0] separator
            Constraint::Length(1), // [1] header
            Constraint::Min(1),    // [2] args
            Constraint::Min(1),    // [3] reason
            Constraint::Length(1), // [4] question
            Constraint::Length(1), // [5] option 1
            Constraint::Length(1), // [6] option 2
            Constraint::Length(1), // [7] option 3
            Constraint::Length(1), // [8] hint
        ]
    };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
}

/// Render the command/args block. For edit/multiedit, render a colored
/// old-to-new diff preview; other tools show the extracted command or raw
/// args. Entitlement asks render their own detail lines.
fn render_args(f: &mut Frame, a: &crate::state::Approval, slot: Rect) {
    let args_value = serde_json::from_str::<Value>(&a.args).ok();
    if a.is_entitlement() {
        f.render_widget(
            Paragraph::new(entitlement_detail(&a.args, args_value.as_ref()))
                .wrap(Wrap { trim: false }),
            slot,
        );
        return;
    }
    let diff_lines = args_value.as_ref().and_then(|v| diff_preview(&a.tool, v));
    match diff_lines {
        Some(lines) => {
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), slot);
        }
        None => {
            let cmd = args_command(&a.tool, args_value.as_ref(), &a.args);
            f.render_widget(
                Paragraph::new(format!("   {cmd}"))
                    .style(Style::new().fg(Color::White))
                    .wrap(Wrap { trim: false }),
                slot,
            );
        }
    }
}

/// Parse the skill name from an entitlement ask's input JSON.
fn entitlement_skill(args: &str) -> Option<String> {
    let v: Value = serde_json::from_str(args).ok()?;
    v.get("skill").and_then(|s| s.as_str()).map(String::from)
}

/// Detail lines for the entitlement card: the skill that was blocked, the
/// command that triggered the denial, and each service the deny-log scan
/// found. The command is shown so the user can judge whether the request
/// is legitimate — a service name alone does not tell the user what the
/// skill was trying to do.
fn entitlement_detail(args: &str, parsed: Option<&Value>) -> Vec<Line<'static>> {
    let skill = entitlement_skill(args).unwrap_or_default();
    let origin = parsed
        .and_then(|v| v.get("origin"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let mut lines = vec![Line::from(format!(
        " Skill {skill} ({origin}) was blocked from"
    ))];
    // Command before services: the command is what the user judges by, so
    // it stays visible when the card's bounded area clips the lower lines.
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

/// Render the verdict options into the three option slots. Display order is
/// Yes then Yes-don't-ask then No; the selected index keeps its internal
/// mapping (0=Yes, 1=No, 2=Yes-don't-ask). A two-option card (protected-path
/// or entitlement) hides Yes-don't-ask and renumbers No to 2.
fn render_options(f: &mut Frame, a: &crate::state::Approval, chunks: &[Rect]) {
    let yes_focused = a.selected == 0;
    let yes_label = if a.is_entitlement() {
        "Always allow"
    } else {
        "Yes"
    };
    f.render_widget(
        Paragraph::new(format!(
            " {} 1. {yes_label}",
            if yes_focused { "❯" } else { " " }
        ))
        .style(if yes_focused {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::White)
        }),
        chunks[5],
    );
    if a.two_option_card() {
        let no_focused = a.selected == 1;
        f.render_widget(
            Paragraph::new(format!(" {} 2. No", if no_focused { "❯" } else { " " })).style(
                if no_focused {
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::White)
                },
            ),
            chunks[6],
        );
        return;
    }
    let dont_ask_focused = a.selected == 2;
    let dont_ask_label = match a.tool.to_ascii_lowercase().as_str() {
        "bash" | "sh" | "exec" | "shell" => {
            let val = serde_json::from_str::<Value>(&a.args).ok();
            let cmd = args_command(&a.tool, val.as_ref(), &a.args);
            cmd.split_whitespace()
                .next()
                .map(|t| format!("{t} *"))
                .unwrap_or_else(|| a.tool.clone())
        }
        _ => a.tool.clone(),
    };
    f.render_widget(
        Paragraph::new(format!(
            " {} 2. Yes, and don't ask again for {}",
            if dont_ask_focused { "❯" } else { " " },
            dont_ask_label,
        ))
        .style(if dont_ask_focused {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::White)
        }),
        chunks[6],
    );
    let no_focused = a.selected == 1;
    f.render_widget(
        Paragraph::new(format!(" {} 3. No", if no_focused { "❯" } else { " " })).style(
            if no_focused {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            },
        ),
        chunks[7],
    );
}

/// A short label for the ask source, rendered as a prefix on the reason
/// line so the user reads which class of check escalated the call. Empty
/// when no source traveled the wire (the generic-prompt path).
fn source_label(a: &crate::state::Approval) -> &'static str {
    use houyicoder_protocol::frontend::permission::AskSource;
    if a.is_entitlement() {
        return "Deny-log discovery";
    }
    match a.source {
        Some(AskSource::SystemSafety) => "Protected path",
        Some(AskSource::Detection) => "Detection",
        Some(AskSource::UserRule) => "Rule",
        Some(AskSource::ToolNative) => "Tool",
        // The wire Unknown fallback (a future engine source) has no label.
        _ => "",
    }
}

/// Extract the human-readable command from the tool args. For bash, the
/// "command" field holds the shell string; for other tools, fall back to
/// the raw args JSON. Returns the raw string when JSON parsing fails.
/// Long commands are tail-truncated so the reason and option lines below
/// stay visible in the card's bounded area.
fn args_command(tool: &str, value: Option<&Value>, raw: &str) -> String {
    let cmd = if let Some(v) = value {
        if let Some(c) = v.get("command").and_then(|c| c.as_str()) {
            c.to_string()
        } else {
            v.to_string()
        }
    } else {
        let _ = tool;
        raw.to_string()
    };
    truncate_tail(&cmd, 80)
}

/// Truncate a string to at most max bytes of the tail, prefixing an
/// ellipsis. Keeps the tail (the end of a path or command is the
/// discriminating part) and drops the head. floor_char_boundary prevents
/// splitting a multi-byte character at the byte offset.
fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let start = s.floor_char_boundary(s.len() - max);
    format!("…{}", &s[start..])
}

/// Capitalize the first character of a tool name ("bash" -> "Bash").
fn cap_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Build a colored old→new diff preview for edit/multiedit. Each old line is
/// red (-), each new line green (+). Returns None for other tools or
/// malformed input (caller falls back to raw args). Multi-line strings split
/// on \n.
fn diff_preview(tool: &str, input: &Value) -> Option<Vec<Line<'static>>> {
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
#[path = "approval_tests.rs"]
mod tests;
