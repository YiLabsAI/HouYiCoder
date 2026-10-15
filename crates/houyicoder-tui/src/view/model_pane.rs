//! /model pane content: a responsive picker over the catalog rows the host
//! reports. The wide layout puts the display name, the id the provider sees
//! and the effective context window on one row per model; the narrow layout
//! keeps one line per model and moves the focused row's id and context into a
//! fixed detail row below the list, so scrolling never shifts the settings.
//!
//! The applied check and the focus marker are separate: the check follows the
//! host's applied selection, the marker follows the cursor.

pub(crate) mod draft;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};
use unicode_width::UnicodeWidthStr;

use crate::session::capabilities::model::ModelCapability;
use crate::state::App;
use crate::state::model_draft::{ModelDraft, ModelSettingFocus};
use crate::view::line_wrap::truncate_width;
use crate::view::navigation::key_hint;
use houyicoder_protocol::frontend::model::{
    ContextWindow, ContextWindowSource, FastModeAvailability, ModelChoice,
};

/// Default height /model asks for: a title, the list, the focus detail on the
/// narrow layout, the two settings, and a footer. Capped at half the main
/// area by draw_command_pane.
pub(crate) const MODEL_PANE_HEIGHT: u16 = 13;

/// The inner width at which the pane switches to the wide layout. Below it the
/// per-row copy is cut back so the settings and the footer keep their rows.
pub(crate) const WIDE_INNER_WIDTH: u16 = 76;

/// The share of a wide row's width the name column takes, in tenths, and the
/// smallest it is allowed to be. The id and context fill what is left.
const WIDE_NAME_TENTHS: usize = 4;
const WIDE_NAME_MIN: usize = 14;

/// The least gap between the name column and the id it prefixes.
const WIDE_GAP: usize = 2;

/// The marker in a row's gutter: the focused row, and the blank gutter that
/// keeps the labels aligned.
const FOCUS_MARKER: &str = "\u{276f} ";
const BLANK_MARKER: &str = "  ";

/// The mark on the row the host reports as applied.
const APPLIED_MARK: &str = " \u{2714}";

/// The display name of the Default sentinel. Picking it resolves the default
/// model, so the row is named for the choice rather than for the resolved id.
const DEFAULT_LABEL: &str = "Default (recommended)";

/// Render the /model content into the Pane inner rect.
pub(crate) fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    let model = &app.sessions.current.capabilities.model;
    let draft = &app.ui.model;
    let wide = inner.width >= WIDE_INNER_WIDTH;
    // A model with no fast tier declared drops the Fast Mode row entirely
    // rather than printing an unavailable line the user never configured.
    let fast_shown = !matches!(
        model.capabilities_at(draft.row).fast,
        FastModeAvailability::NotConfigured
    );
    let mut constraints = vec![Constraint::Length(1), Constraint::Min(0)];
    if !wide {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1));
    if fast_shown {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Select a model",
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        )])),
        chunks[0],
    );
    draw_list(f, chunks[1], model, draft, wide);
    let mut idx = 2;
    if !wide {
        let detail = focused_detail(model, draft, chunks[idx].width as usize);
        f.render_widget(Paragraph::new(detail), chunks[idx]);
        idx += 1;
    }
    f.render_widget(Paragraph::new(effort_line(model, draft)), chunks[idx]);
    idx += 1;
    if fast_shown {
        f.render_widget(Paragraph::new(fast_line(model, draft)), chunks[idx]);
        idx += 1;
    }
    f.render_widget(
        Paragraph::new(footer_line(model, wide, fast_shown)),
        chunks[idx],
    );
}

/// The model list, scrolled by the list state so the title, the detail and
/// the settings keep their rows however long the catalog is.
fn draw_list(f: &mut Frame, area: Rect, model: &ModelCapability, draft: &ModelDraft, wide: bool) {
    if model.catalog.entries.is_empty() {
        // No rows to pick from; the guidance belongs to the list body, not
        // the footer, so a narrow width cannot truncate the escape hint.
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no catalog configured; add model.catalog entries to settings.json",
                Style::new().fg(Color::DarkGray),
            ))),
            area,
        );
        return;
    }
    let width = area.width as usize;
    let items: Vec<ListItem> = (0..model.rows())
        .map(|row| ListItem::new(row_line(model, draft, row, wide, width)))
        .collect();
    let mut state = ListState::default();
    state.select(Some(draft.row.min(model.rows().saturating_sub(1))));
    let list = List::new(items)
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(list, area, &mut state);
}

/// One row: on the wide layout the name column is padded so the ids line up;
/// on the narrow layout the row is the name alone, cut to the width.
fn row_line(
    model: &ModelCapability,
    draft: &ModelDraft,
    row: usize,
    wide: bool,
    width: usize,
) -> Line<'static> {
    let (left, style) = row_left(model, draft, row);
    if !wide {
        return Line::from(Span::styled(truncate_width(&left, width), style));
    }
    let name_col = (width * WIDE_NAME_TENTHS / 10).max(WIDE_NAME_MIN);
    let pad = name_col.saturating_sub(UnicodeWidthStr::width(left.as_str()));
    let pad = pad.max(WIDE_GAP);
    let taken = UnicodeWidthStr::width(left.as_str()) + pad;
    let right = truncate_width(&row_right(model, row), width.saturating_sub(taken));
    Line::from(vec![
        Span::styled(format!("{left}{}", " ".repeat(pad)), style),
        Span::styled(right, Style::new().fg(Color::DarkGray)),
    ])
}

/// The left column of a row: the gutter marker, the number, the label and the
/// applied mark, plus the style the whole column takes.
fn row_left(model: &ModelCapability, draft: &ModelDraft, row: usize) -> (String, Style) {
    let marker = if row == draft.row {
        FOCUS_MARKER
    } else {
        BLANK_MARKER
    };
    let (label, applied) = match model.entry_at(row) {
        Some(entry) => (
            entry.label().to_string(),
            model.catalog.selected
                == ModelChoice::Explicit {
                    id: entry.id.clone(),
                },
        ),
        None => (
            DEFAULT_LABEL.to_string(),
            model.catalog.selected == ModelChoice::Default,
        ),
    };
    let mark = if applied { APPLIED_MARK } else { "" };
    let style = if applied {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::White)
    };
    (format!("{marker}{}. {label}{mark}", row + 1), style)
}

/// The right column of a wide row: the id the provider sees and the effective
/// context window.
fn row_right(model: &ModelCapability, row: usize) -> String {
    let (id, window) = match model.entry_at(row) {
        Some(entry) => (entry.id.as_str(), entry.capabilities.context_window),
        None => (
            model.catalog.resolved_default.id.as_str(),
            model.catalog.resolved_default.capabilities.context_window,
        ),
    };
    match context_label(window) {
        Some(context) => format!("{id} · {context} context"),
        None => id.to_string(),
    }
}

/// The fixed detail row under the narrow list: the focused model's id and
/// context always, preceded by as much of the description, the output cap and
/// the window provenance as the width allows. Nothing here belongs to the
/// applied model, so moving the focus swaps the text in place.
fn focused_detail(model: &ModelCapability, draft: &ModelDraft, width: usize) -> Line<'static> {
    let default = &model.catalog.resolved_default;
    let focused = model.entry_at(draft.row);
    let (id, max_output, window) = match focused {
        Some(entry) => (
            entry.id.as_str(),
            entry.capabilities.max_output_tokens,
            entry.capabilities.context_window,
        ),
        None => (
            default.id.as_str(),
            default.capabilities.max_output_tokens,
            default.capabilities.context_window,
        ),
    };
    let context = context_label(window);
    let text = compose_detail(
        focused.and_then(|e| e.description.as_deref()),
        max_output,
        window,
        id,
        context.as_deref(),
        width,
    );
    Line::from(Span::styled(text, Style::new().fg(Color::DarkGray)))
}

/// Build the detail text, dropping copy in the order the pane degrades: the
/// description, then the output cap, then the window provenance. The id and
/// the context number are never dropped, and the word that says what the
/// number is goes last of all.
fn compose_detail(
    description: Option<&str>,
    max_output: Option<u32>,
    window: Option<ContextWindow>,
    id: &str,
    context: Option<&str>,
    width: usize,
) -> String {
    let mut prefixes: Vec<String> = Vec::new();
    if let Some(description) = description {
        prefixes.push(description.to_string());
    }
    if let Some(max) = max_output {
        prefixes.push(format!("{max} max out"));
    }
    if let Some(window) = window {
        prefixes.push(source_label(window.source).to_string());
    }
    let head = match context {
        Some(context) => format!("{id} · {context}"),
        None => id.to_string(),
    };
    let sources: [Option<String>; 2] = [
        context.is_some().then(|| format!("{head} context")),
        Some(head.clone()),
    ];
    // The tail outranks the prefixes: a candidate drops the provenance copy to
    // keep the word that gives the window number its meaning, never the other
    // way round.
    for tail in sources.iter().flatten() {
        for start in 0..=prefixes.len() {
            let candidate = join_segments(&prefixes[start..], tail);
            if UnicodeWidthStr::width(candidate.as_str()) <= width {
                return candidate;
            }
        }
    }
    truncate_width(&head, width)
}

/// Join the surviving detail segments with the separator the pane uses.
fn join_segments(prefixes: &[String], tail: &str) -> String {
    let mut parts: Vec<&str> = prefixes.iter().map(String::as_str).collect();
    parts.push(tail);
    parts.join(" · ")
}

/// The Reasoning Effort setting: the level the draft would send, marked as the
/// chain's default until the user adjusts it. A model that speaks no effort
/// dialect says so rather than offering levels that would not be sent.
fn effort_line(model: &ModelCapability, draft: &ModelDraft) -> Line<'static> {
    let label = setting_label(
        "Reasoning Effort:",
        draft.focus == ModelSettingFocus::Effort,
    );
    let value = if !model.effort_supported_at(draft.row) {
        "unavailable".to_string()
    } else {
        // The marker keys on the value the chain resolves for this row, not on
        // whether the user has touched effort: cycling away and back to the
        // chain's value re-collects it rather than losing the marker for good.
        // An auto pick (None) is the default state by definition — no pin
        // means follow the chain — so it carries the marker regardless of
        // which concrete level the chain resolves to.
        let marker = if draft.effort.is_none() || draft.effort == model.chain_effort(draft.row) {
            " (default)"
        } else {
            ""
        };
        match draft.effort {
            Some(level) => format!("{}{marker}", level.label()),
            None => format!("auto{marker}"),
        }
    };
    Line::from(vec![
        Span::raw("  "),
        label,
        Span::styled(value, Style::new().fg(Color::White)),
    ])
}

/// The Fast Mode setting: the tier the draft would send, the reason a target
/// model forces it off, or why the setting cannot be taken at all.
fn fast_line(model: &ModelCapability, draft: &ModelDraft) -> Line<'static> {
    let label = setting_label("Fast Mode:", draft.focus == ModelSettingFocus::Fast);
    let availability = model.capabilities_at(draft.row).fast;
    let value = if draft.fast_forced_off(availability.is_available()) {
        "off (required by selected model)".to_string()
    } else if availability.is_available() {
        draft.speed.label().to_string()
    } else {
        match availability.reason() {
            Some(reason) => format!("unavailable ({reason})"),
            None => "unavailable".to_string(),
        }
    };
    let style = if draft.focus == ModelSettingFocus::Fast {
        Style::new().fg(Color::White)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Line::from(vec![Span::raw("  "), label, Span::styled(value, style)])
}

/// A setting name, highlighted while it holds the arrows.
fn setting_label(name: &'static str, focused: bool) -> Span<'static> {
    let style = if focused {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Span::styled(format!("{name} "), style)
}

/// The key hints. The pane has no save key of its own: Enter commits the
/// draft, and a pick that fails keeps the pane open for another try. While a
/// commit is with the host Enter and Esc are held, so the footer says the
/// save is in flight instead of promising keys that do nothing. On a narrow
/// pane only the commit and cancel hints survive the width; those two must
/// never be the ones truncated away.
fn footer_line(model: &ModelCapability, wide: bool, fast_shown: bool) -> Line<'static> {
    if model.is_pending() {
        return Line::from(Span::styled(
            "saving\u{2026}",
            Style::new().fg(Color::Yellow),
        ));
    }
    if model.catalog.entries.is_empty() {
        // Short so a narrow width cannot truncate the Esc hint; the
        // configuration guidance lives in the list body above.
        let mut line = Line::from(Span::styled(
            "No models configured · ",
            Style::new().fg(Color::DarkGray),
        ));
        line.spans.extend(key_hint(&[("Esc", "cancel")]).spans);
        return line;
    }
    if wide {
        // The Tab hint is only honest while a second adjustable setting is on
        // screen; with Fast Mode hidden, the arrows on effort need no hop.
        if fast_shown {
            key_hint(&[
                ("Tab", "setting"),
                ("Left/Right", "adjust"),
                ("Up/Down", "select"),
                ("Enter", "save"),
                ("Esc", "cancel"),
            ])
        } else {
            key_hint(&[
                ("Left/Right", "adjust"),
                ("Up/Down", "select"),
                ("Enter", "save"),
                ("Esc", "cancel"),
            ])
        }
    } else {
        key_hint(&[("Enter", "save"), ("Esc", "cancel")])
    }
}

/// The token count as the pane prints it. None when the window is unknown, so
/// the row shows the id alone rather than a zero.
fn context_label(window: Option<ContextWindow>) -> Option<String> {
    let window = window?;
    if window.tokens == 0 {
        return None;
    }
    let tokens = u64::from(window.tokens);
    Some(if tokens >= 1_000_000 {
        format!("{}M", tokens / 1_000_000)
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    })
}

/// Where the window came from, dropped last when the detail row runs out of
/// width.
fn source_label(source: ContextWindowSource) -> &'static str {
    match source {
        ContextWindowSource::Provider => "from provider",
        ContextWindowSource::Learned => "learned limit",
        ContextWindowSource::ExplicitConfig => "from settings",
        ContextWindowSource::ModelSuffix => "from model suffix",
        ContextWindowSource::ModelCatalog => "from model table",
        ContextWindowSource::Fallback => "default window",
    }
}

#[cfg(test)]
mod tests;
