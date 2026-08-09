//! AskUserQuestion card rendered inline at the transcript tail. Renders a
//! navigation bar (per-question tabs with checkboxes + a Submit tab), one
//! question at a time with numbered options and a cursor marker, or the
//! submit (review) view listing answers with a Submit-answers/Cancel select.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::records::{AskQuestion, QuestionCard};
use crate::state::App;

/// Render the AskUserQuestion card inline at the transcript tail.
pub fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(aq) = app.ask_question.as_ref() else {
        return;
    };
    f.render_widget(Clear, area);

    if aq.is_submit_view() {
        draw_submit_view(f, aq, area);
    } else if let Some(q) = aq.questions.get(aq.current) {
        draw_question_view(f, aq, q, area);
    }
}

/// Render a single question view: separator, nav bar, header chip, question
/// text, options (incl Other), multi-select Submit/Next button, hint.
fn draw_question_view(f: &mut Frame, aq: &AskQuestion, q: &QuestionCard, area: Rect) {
    let option_count = q.options.len() + 1; // +1 for Other
    let has_nav = !aq.hide_submit_tab();
    let has_submit_btn = q.multi_select;
    let chunks = build_question_layout(area, option_count, has_nav, has_submit_btn);
    let mut idx = 0;

    // Separator.
    let sep: String = "-".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(sep).style(Style::new().fg(Color::DarkGray)),
        chunks[idx],
    );
    idx += 1;

    // Navigation bar.
    if has_nav {
        draw_nav_bar(f, aq, chunks[idx]);
        idx += 1;
    }

    // Header chip.
    let header = truncate_header(&q.header);
    f.render_widget(
        Paragraph::new(format!(" [{header}]"))
            .style(Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        chunks[idx],
    );
    idx += 1;

    // Question text.
    f.render_widget(
        Paragraph::new(format!(" {}", q.question))
            .style(Style::new().fg(Color::White))
            .wrap(Wrap { trim: false }),
        chunks[idx],
    );
    idx += 1;

    // Gap.
    idx += 1;

    let focused = aq.cursors.get(aq.current).copied().unwrap_or(0);

    // Options.
    for (i, opt) in q.options.iter().enumerate() {
        let is_focused = i == focused && !aq.other_focused;
        let is_selected = aq.selections[aq.current].contains(&i);
        draw_option_row(
            f,
            chunks[idx],
            i + 1,
            &opt.label,
            &opt.description,
            is_focused,
            is_selected,
        );
        idx += 1;
    }

    // Other option (auto-appended).
    draw_other_option(f, aq, focused, chunks[idx]);
    idx += 1;

    // Multi-select Submit/Next button.
    if has_submit_btn {
        let btn_idx = aq.current_submit_btn_idx();
        let is_focused = focused == btn_idx && !aq.other_focused;
        let label = aq.submit_btn_label();
        let marker = if is_focused { ">" } else { " " };
        let line = format!(" {marker}       [{label}]");
        let style = if is_focused {
            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(Color::DarkGray)
        };
        f.render_widget(Paragraph::new(line).style(style), chunks[idx]);
        idx += 1;
    }

    // Gap.
    idx += 1;

    // Hint.
    let hint = nav_hint(aq, q);
    f.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        chunks[idx],
    );
}

/// Render the submit (review) view: separator, nav bar, title, warning,
/// answer list, and Submit-answers/Cancel select.
fn draw_submit_view(f: &mut Frame, aq: &AskQuestion, area: Rect) {
    let n_questions = aq.questions.len();
    let chunks = build_submit_layout(area, n_questions);
    let mut idx = 0;

    // Separator.
    let sep: String = "-".repeat(area.width as usize);
    f.render_widget(
        Paragraph::new(sep).style(Style::new().fg(Color::DarkGray)),
        chunks[idx],
    );
    idx += 1;

    // Nav bar.
    if !aq.hide_submit_tab() {
        draw_nav_bar(f, aq, chunks[idx]);
        idx += 1;
    }

    // Title.
    f.render_widget(
        Paragraph::new(" Review your answers").style(Style::new().fg(Color::White)),
        chunks[idx],
    );
    idx += 1;

    // Warning if not all answered.
    if !aq.all_answered() {
        f.render_widget(
            Paragraph::new(" ! You have not answered all questions")
                .style(Style::new().fg(Color::Yellow)),
            chunks[idx],
        );
        idx += 1;
    }

    // Answer list: one line per question (bullet + question, arrow + answer).
    let answers = aq.build_answers();
    for q in &aq.questions {
        let answer_text = answers
            .get(&q.question)
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let display = if answer_text.is_empty() {
            "(no answer)".to_string()
        } else {
            answer_text.to_string()
        };
        f.render_widget(
            Paragraph::new(format!(" * {}", q.question)).style(Style::new().fg(Color::White)),
            chunks[idx],
        );
        idx += 1;
        f.render_widget(
            Paragraph::new(format!("   -> {display}")).style(Style::new().fg(Color::Green)),
            chunks[idx],
        );
        idx += 1;
    }

    // Prompt + Submit/Cancel select.
    let cursor = aq.submit_cursor;
    let submit_marker = if cursor == 0 { ">" } else { " " };
    let cancel_marker = if cursor == 1 { ">" } else { " " };
    let submit_style = if cursor == 0 {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::White)
    };
    let cancel_style = if cursor == 1 {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::White)
    };
    f.render_widget(
        Paragraph::new(format!(" {submit_marker} Submit answers")).style(submit_style),
        chunks[idx],
    );
    idx += 1;
    f.render_widget(
        Paragraph::new(format!(" {cancel_marker} Cancel")).style(cancel_style),
        chunks[idx],
    );
}

/// Draw the navigation bar: left arrow, per-question tabs (checkbox +
/// truncated header), Submit tab. Hidden for single single-select question.
fn draw_nav_bar(f: &mut Frame, aq: &AskQuestion, area: Rect) {
    let left = if aq.current == 0 {
        " <".to_string()
    } else {
        "<".to_string()
    };
    let left_style = if aq.current == 0 {
        Style::new().fg(Color::DarkGray)
    } else {
        Style::new().fg(Color::White)
    };

    let mut line = String::new();
    line.push_str(&left);
    line.push(' ');

    for (i, q) in aq.questions.iter().enumerate() {
        let is_answered = aq.is_answered(i);
        let checkbox = if is_answered { "[x]" } else { "[ ]" };
        let header = truncate_header(&q.header);
        line.push(' ');
        line.push_str(checkbox);
        line.push(' ');
        line.push_str(&header);
        line.push(' ');
    }

    // Submit tab.
    if aq.is_submit_view() {
        line.push_str(" [v] Submit ");
    } else {
        line.push_str(" [ ] Submit ");
    }

    // Right arrow.
    if aq.current >= aq.max_index() {
        line.push_str(" >");
    } else {
        line.push('>');
    }

    let right_style = if aq.current >= aq.max_index() {
        Style::new().fg(Color::DarkGray)
    } else {
        Style::new().fg(Color::White)
    };

    // Build the full line; highlight the current tab with inverse style.
    // For simplicity, render the whole bar in one paragraph. The current tab
    // highlight is indicated by the checkbox state (answered = [x]).
    let full_style = if aq.is_submit_view() {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::White)
    };
    let _ = (left_style, right_style);
    f.render_widget(Paragraph::new(line).style(full_style), area);
}

/// Draw a numbered option row with cursor marker and checkbox.
fn draw_option_row(
    f: &mut Frame,
    area: Rect,
    num: usize,
    label: &str,
    description: &str,
    is_focused: bool,
    is_selected: bool,
) {
    let marker = if is_focused { ">" } else { " " };
    let check = if is_selected { "[x]" } else { "[ ]" };
    let body = if is_focused && !description.is_empty() {
        format!("{num}. {label} -- {description}")
    } else {
        format!("{num}. {label}")
    };
    let line = format!(" {marker} {check} {body}");
    let style = if is_focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::White)
    };
    f.render_widget(Paragraph::new(line).style(style), area);
}

/// Draw the auto-appended Other option row.
fn draw_other_option(f: &mut Frame, aq: &AskQuestion, focused: usize, area: Rect) {
    let other_idx = aq.current_other_idx();
    let other_focused = focused == other_idx || aq.other_focused;
    let other_selected = aq.other_is_selected(aq.current);
    let other_marker = if other_focused { ">" } else { " " };
    let other_check = if other_selected { "[x]" } else { "[ ]" };
    let other_text = aq
        .other_text
        .get(aq.current)
        .and_then(|t| t.as_ref())
        .map(|s| s.as_str())
        .unwrap_or("");
    // The row shows a placeholder when no text is typed: a trailing period
    // for single-select, none for multi. The "Other" schema label is a
    // sentinel, never a display string — the placeholder renders
    // (dim when unfocused and empty, a ghost with cursor when focused, the
    // typed text once the user has typed).
    let multi = aq
        .questions
        .get(aq.current)
        .map(|q| q.multi_select)
        .unwrap_or(false);
    let placeholder = if multi {
        "Type something"
    } else {
        "Type something."
    };
    let other_label = if !other_text.is_empty() {
        if aq.other_focused {
            format!("{other_text}_")
        } else {
            other_text.to_string()
        }
    } else if aq.other_focused {
        format!("{placeholder}_")
    } else {
        placeholder.to_string()
    };
    let other_line = format!(
        " {other_marker} {other_check} {}. {other_label}",
        other_idx + 1
    );
    let other_style = if other_focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else if !other_text.is_empty() {
        Style::new().fg(Color::White)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    f.render_widget(Paragraph::new(other_line).style(other_style), area);
}

/// Build the vertical layout for a question view: separator + nav_bar +
/// header + question + gap + options + submit_btn + gap + hint.
fn build_question_layout(
    area: Rect,
    option_count: usize,
    has_nav: bool,
    has_submit_btn: bool,
) -> Vec<Rect> {
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // separator
    ];
    if has_nav {
        constraints.push(Constraint::Length(1)); // nav bar
    }
    constraints.push(Constraint::Length(1)); // header
    constraints.push(Constraint::Length(1)); // question
    constraints.push(Constraint::Length(1)); // gap
    for _ in 0..option_count {
        constraints.push(Constraint::Length(1)); // option rows
    }
    if has_submit_btn {
        constraints.push(Constraint::Length(1)); // submit button
    }
    constraints.push(Constraint::Length(1)); // gap
    constraints.push(Constraint::Length(1)); // hint
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

/// Build the vertical layout for the submit view: separator + nav_bar +
/// title + warning + answers(2 per question) + submit + cancel.
fn build_submit_layout(area: Rect, n_questions: usize) -> Vec<Rect> {
    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // separator
        Constraint::Length(1), // nav bar
        Constraint::Length(1), // title
        Constraint::Length(1), // warning
    ];
    for _ in 0..n_questions {
        // Two lines per question: bullet line + answer line.
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // submit row
    constraints.push(Constraint::Length(1)); // cancel row
    Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area)
        .to_vec()
}

/// Truncate the header to 12 characters (the schema max width).
fn truncate_header(header: &str) -> String {
    if header.len() > 12 {
        header.chars().take(12).collect()
    } else {
        header.to_string()
    }
}

/// Navigation hint string varies by question type and count.
fn nav_hint(aq: &AskQuestion, q: &QuestionCard) -> &'static str {
    if q.multi_select {
        " up/down navigate - enter toggle - tab/arrows switch - esc cancel"
    } else if aq.questions.len() == 1 {
        " up/down navigate - enter select - esc cancel"
    } else {
        " up/down navigate - enter select - tab/arrows switch - esc cancel"
    }
}
