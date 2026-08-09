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

    // Layout (top-down): separator(1) + header(1) + gap(1) + args(Min) +
    // reason(2: detail + optional containment note) + gap(1) + question(1)
    // + yes(1) + no-or-dontask(1) + no-or-blank(1) + hint(1).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);

    // Top separator: full-width thin horizontal rule.
    let sep: String = "─".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(sep).style(Style::new().fg(Color::DarkGray)),
        chunks[0],
    );

    // Header: "<Tool> command"
    f.render_widget(
        Paragraph::new(format!(" {} command", cap_first(&a.tool)))
            .style(Style::new().fg(Color::White).add_modifier(Modifier::BOLD)),
        chunks[1],
    );

    // Command/args block (indented). For edit/multiedit, render a colored
    // old→new diff preview; other tools show the extracted command or raw
    // args.
    let args_value = serde_json::from_str::<Value>(&a.args).ok();
    let diff_lines = args_value.as_ref().and_then(|v| diff_preview(&a.tool, v));
    match diff_lines {
        Some(lines) => {
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), chunks[3]);
        }
        None => {
            let cmd = args_command(&a.tool, args_value.as_ref(), &a.args);
            f.render_widget(
                Paragraph::new(format!("   {cmd}"))
                    .style(Style::new().fg(Color::White))
                    .wrap(Wrap { trim: false }),
                chunks[3],
            );
        }
    }

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
        chunks[4],
    );

    // Question
    f.render_widget(Paragraph::new(" Do you want to proceed?"), chunks[6]);

    // Numbered options with a cursor marker on the focused one. A protected-
    // path ask hides Yes-don't-ask (consent cannot override it) and renumbers
    // No to 2; otherwise the built-in three-option set applies.
    render_options(f, a, &chunks);

    // Bottom hint
    let hint = if a.remember_hidden() {
        " ↑↓ navigate · 1/2 select · Enter confirm · Esc cancel"
    } else {
        " ↑↓ navigate · 1/2/3 select · Enter confirm · Esc cancel"
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        chunks[10],
    );
}

/// Render the verdict options into the three option slots. Display order is
/// Yes then Yes-don't-ask then No; the selected index keeps its internal
/// mapping (0=Yes, 1=No, 2=Yes-don't-ask). A protected-path ask hides
/// Yes-don't-ask and renumbers No to 2.
fn render_options(f: &mut Frame, a: &crate::state::Approval, chunks: &[Rect]) {
    let yes_focused = a.selected == 0;
    f.render_widget(
        Paragraph::new(format!(" {} 1. Yes", if yes_focused { "❯" } else { " " })).style(
            if yes_focused {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            },
        ),
        chunks[7],
    );
    if a.remember_hidden() {
        let no_focused = a.selected == 1;
        f.render_widget(
            Paragraph::new(format!(" {} 2. No", if no_focused { "❯" } else { " " })).style(
                if no_focused {
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::White)
                },
            ),
            chunks[8],
        );
        // chunks[9] (the No slot in the 3-option layout) stays blank.
        return;
    }
    let dont_ask_focused = a.selected == 2;
    f.render_widget(
        Paragraph::new(format!(
            " {} 2. Yes, and don't ask again for {}",
            if dont_ask_focused { "❯" } else { " " },
            a.tool,
        ))
        .style(if dont_ask_focused {
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::White)
        }),
        chunks[8],
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
        chunks[9],
    );
}

/// A short label for the ask source, rendered as a prefix on the reason
/// line so the user reads which class of check escalated the call. Empty
/// when no source traveled the wire (the generic-prompt path).
fn source_label(a: &crate::state::Approval) -> &'static str {
    use houyicoder_protocol::frontend::permission::AskSource;
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
fn args_command(tool: &str, value: Option<&Value>, raw: &str) -> String {
    if let Some(v) = value {
        if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
            return cmd.to_string();
        }
        return v.to_string();
    }
    let _ = tool;
    raw.to_string()
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
mod tests {
    use super::{cap_first, diff_preview};
    use crate::composition;
    use crate::test_support::render_text;
    use serde_json::json;

    #[test]
    fn test_diff_preview_edit_red() {
        let v = json!({"path": "a.rs", "old_string": "foo()", "new_string": "bar()"});
        let lines = diff_preview("edit", &v).expect("preview");
        // one old line (-foo()) + one new line (+bar()) = 2.
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_diff_preview_multiedit_multi() {
        let v = json!({"path": "a.rs", "edits": [
            {"old_string": "a", "new_string": "b"},
            {"old_string": "c", "new_string": "d"}
        ]});
        let lines = diff_preview("multiedit", &v).expect("preview");
        // 2 "edit N:" headers + 2 old + 2 new = 6.
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn test_diff_preview_other_tool() {
        assert!(diff_preview("bash", &json!({"command": "ls"})).is_none());
    }

    #[test]
    fn test_diff_preview_malformed_none() {
        assert!(diff_preview("edit", &json!({"path": "a.rs"})).is_none());
    }

    #[test]
    fn test_cap_first_uppercases() {
        assert_eq!(cap_first("bash"), "Bash");
        assert_eq!(cap_first("edit"), "Edit");
    }

    #[test]
    fn test_cap_first_empty() {
        assert_eq!(cap_first(""), "");
    }

    #[test]
    fn test_render_no_heavy_border() {
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        app.approval = Some(crate::state::Approval {
            tool: "bash".into(),
            args: r#"{"command":"find . -type f | wc -l"}"#.into(),
            reason: "agent wants to run this tool".into(),
            selected: 0,
            call_id: String::new(),
            options: Vec::new(),
            ..Default::default()
        });
        let out = render_text(&app, 80, 24);
        // The heavy double-border box chars must not appear.
        for ch in ['╔', '╗', '╚', '╝', '║', '═'] {
            assert!(
                !out.contains(ch),
                "heavy border char '{ch}' should be gone, got:\n{out}"
            );
        }
    }

    #[test]
    fn test_render_separator_and_question() {
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        app.approval = Some(crate::state::Approval {
            tool: "bash".into(),
            args: r#"{"command":"find . -type f | wc -l"}"#.into(),
            reason: "agent wants to run this tool".into(),
            selected: 0,
            call_id: String::new(),
            options: Vec::new(),
            ..Default::default()
        });
        let out = render_text(&app, 80, 24);
        // Thin separator line present.
        assert!(out.contains('─'), "separator line missing:\n{out}");
        // Proceed question present.
        assert!(
            out.contains("Do you want to proceed?"),
            "proceed question missing:\n{out}"
        );
        // Numbered Yes/Yes-don't-ask/No options present (aligned order).
        assert!(out.contains("1. Yes"), "Yes option missing:\n{out}");
        assert!(
            out.contains("2. Yes, and don't ask again"),
            "dont-ask option missing:\n{out}"
        );
        assert!(out.contains("3. No"), "No option missing:\n{out}");
        // Cursor marker on the focused (Yes) option.
        assert!(out.contains('❯'), "cursor marker missing:\n{out}");
        // Bottom hint present.
        assert!(out.contains("Esc cancel"), "hint line missing:\n{out}");
        // The actual command text is shown (not just raw JSON).
        assert!(
            out.contains("find . -type f | wc -l"),
            "command text missing:\n{out}"
        );
    }

    #[test]
    fn test_render_cursor_on_reject() {
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        app.approval = Some(crate::state::Approval {
            tool: "bash".into(),
            args: r#"{"command":"ls"}"#.into(),
            reason: "test".into(),
            selected: 1,
            call_id: String::new(),
            options: Vec::new(),
            ..Default::default()
        });
        let out = render_text(&app, 80, 24);
        let lines: Vec<&str> = out.lines().collect();
        // Find the Yes line and the No line. The cursor marker should be
        // on the No line (selected=1), not on the Yes line.
        let yes_line = lines
            .iter()
            .find(|l| l.contains("1. Yes"))
            .expect("Yes line");
        let no_line = lines.iter().find(|l| l.contains("3. No")).expect("No line");
        assert!(
            !yes_line.contains('❯'),
            "cursor should not be on Yes:\n{out}"
        );
        assert!(no_line.contains('❯'), "cursor should be on No:\n{out}");
    }

    #[test]
    fn test_render_cursor_dont_ask() {
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        app.approval = Some(crate::state::Approval {
            tool: "bash".into(),
            args: r#"{"command":"ls"}"#.into(),
            reason: "test".into(),
            selected: 2,
            call_id: String::new(),
            options: Vec::new(),
            ..Default::default()
        });
        let out = render_text(&app, 80, 24);
        let lines: Vec<&str> = out.lines().collect();
        let yes_line = lines
            .iter()
            .find(|l| l.contains("1. Yes"))
            .expect("Yes line");
        let dont_ask_line = lines
            .iter()
            .find(|l| l.contains("2. Yes, and don't ask again"))
            .expect("dont-ask line");
        assert!(
            !yes_line.contains('❯'),
            "cursor should not be on Yes:\n{out}"
        );
        assert!(
            dont_ask_line.contains('❯'),
            "cursor should be on dont-ask:\n{out}"
        );
    }

    /// The reason the gate produced renders as the card's reason line, so the
    /// user reads why they are being asked instead of a generic prompt.
    #[test]
    fn test_renders_gate_reason_detail() {
        use houyicoder_protocol::frontend::permission::AskSource;
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        app.approval = Some(crate::state::Approval {
            tool: "bash".into(),
            args: r#"{"command":"rm -rf x"}"#.into(),
            reason: "rm needs confirmation".into(),
            source: Some(AskSource::Detection),
            selected: 0,
            call_id: String::new(),
            options: Vec::new(),
            ..Default::default()
        });
        let out = render_text(&app, 80, 24);
        assert!(
            out.contains("Detection: rm needs confirmation"),
            "source label + reason detail must render: {out}"
        );
        assert!(
            !out.contains("agent wants to run this tool"),
            "generic fallback must not show when a reason is present: {out}"
        );
    }

    /// A containment note renders on its own line beneath the reason.
    #[test]
    fn test_renders_containment_note_line() {
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        app.approval = Some(crate::state::Approval {
            tool: "bash".into(),
            args: r#"{"command":"curl x"}"#.into(),
            reason: "network egress".into(),
            containment_note: Some("the sandbox will block this".into()),
            selected: 0,
            call_id: String::new(),
            options: Vec::new(),
            ..Default::default()
        });
        let out = render_text(&app, 80, 24);
        assert!(
            out.contains("the sandbox will block this"),
            "containment note must render on its own line: {out}"
        );
    }

    /// A protected-path ask (source SystemSafety) hides the "Yes, and don't
    /// ask again" option and renumbers No to 2: consent cannot override a
    /// safety check, so the choice is not offered.
    #[test]
    fn test_system_safety_hides_option() {
        use houyicoder_protocol::frontend::permission::AskSource;
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        app.approval = Some(crate::state::Approval {
            tool: "edit".into(),
            args: r#"{"path":".git/config"}"#.into(),
            reason: "protected path".into(),
            source: Some(AskSource::SystemSafety),
            selected: 0,
            call_id: String::new(),
            options: Vec::new(),
            ..Default::default()
        });
        let out = render_text(&app, 80, 24);
        assert!(
            !out.contains("don't ask again"),
            "remember option must be hidden for a protected-path ask: {out}"
        );
        // No is renumbered to 2 (display), not 3.
        assert!(out.contains("2. No"), "No must be renumbered to 2: {out}");
        assert!(
            !out.contains("3. No"),
            "stale 3. No must not show when remember is hidden: {out}"
        );
        // Hint reflects the two-option layout.
        assert!(
            !out.contains("1/2/3 select"),
            "three-option hint must not show when remember is hidden: {out}"
        );
        assert!(out.contains("1/2 select"), "two-option hint missing: {out}");
    }

    /// A detection-sourced ask (not SystemSafety) keeps the remember option.
    #[test]
    fn test_non_safety_keeps_option() {
        use houyicoder_protocol::frontend::permission::AskSource;
        let mut app = composition::app();
        app.screen = crate::state::Screen::Working;
        app.approval = Some(crate::state::Approval {
            tool: "bash".into(),
            args: r#"{"command":"rm x"}"#.into(),
            reason: "rm needs confirmation".into(),
            source: Some(AskSource::Detection),
            selected: 0,
            call_id: String::new(),
            options: Vec::new(),
            ..Default::default()
        });
        let out = render_text(&app, 80, 24);
        assert!(
            out.contains("don't ask again"),
            "remember option must show for a non-safety ask: {out}"
        );
        assert!(
            out.contains("3. No"),
            "No must stay 3 when remember shows: {out}"
        );
    }
}
