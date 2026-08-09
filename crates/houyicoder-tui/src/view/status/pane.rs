//! /status pane content: renders the Status / Config / Usage sub-tabs into
//! the shared Pane template below the transcript tail. The Status tab's field
//! logic is shared with render_status (the String path the stub /status and
//! the unit tests exercise); this module wraps those lines in a sub-tab header
//! + a footer so the pane is a live surface, not a transcript dump. A
//! Settings-modal-style tabbed status minus the Stats tab. The sub-tab
//! cycles with the Tab, Left, or Right keys.
#![allow(clippy::doc_lazy_continuation)]

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::state::{App, enums::StatusTab};

/// Default height /status asks for: a header + up to ~12 status lines + a
/// footer. Capped at half the main area by draw_command_pane.
pub(crate) const STATUS_PANE_HEIGHT: u16 = 20;

/// Render the /status content into the Pane inner rect (the closure passed
/// to draw_command_pane). Sub-tab header + the active tab's content + footer.
pub(crate) fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(inner);
    // Sub-tab header: Status / Config / Usage, the active one bold cyan.
    f.render_widget(tab_header(app.status_tab), chunks[0]);
    // Body: the active tab's content.
    draw_tab_body(f, app.status_tab, chunks[1], app);
    // Footer: the Esc hint (the confirm:no keybinding).
    let footer =
        Paragraph::new("Esc to close · Tab to switch tab").style(Style::new().fg(Color::DarkGray));
    f.render_widget(footer, chunks[2]);
}

/// The body of the active sub-tab. Status reuses render_status (the shared
/// String path + its unit-test authority); Config shows the sandbox / mode /
/// model configuration; Usage shows the token breakdown. When the user is
/// editing the session name on the Status tab, the name row is spliced into
/// an editable line with an inverted caret (the rest of the body is static).
fn draw_tab_body(f: &mut Frame, tab: StatusTab, area: Rect, app: &App) {
    let body: String = match tab {
        StatusTab::Status => {
            let snap = app.snapshot_or_stub();
            crate::command::render::render_status(
                &snap,
                &app.session_id,
                &app.status.sandbox,
                &app.todos_cache,
            )
        }
        StatusTab::Config => render_config(app),
        StatusTab::Usage => render_usage(app),
    };
    let mut lines: Vec<Line<'static>> = body.lines().map(|l| Line::from(l.to_string())).collect();
    // Splice the name row on the Status tab: an inline hint when browsing
    // (e to rename), an editable line with a caret when editing. The name
    // line is the one whose label starts with "Session name"; finding it
    // (rather than assuming a position) stays robust to render_status
    // reordering its rows.
    if tab == StatusTab::Status
        && let Some(idx) = lines.iter().position(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim_start()
                .starts_with("Session name")
        })
    {
        match app.status_name_edit.as_ref() {
            // Editing: replace the row with the editable caret line + a hint.
            Some(field) => lines[idx] = name_edit_line(field),
            // Browsing: append a dim "e to rename" hint to the static row.
            None => lines[idx] = name_hint_line(std::mem::take(&mut lines[idx])),
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// Append a dim "e to rename" hint to the static name row so the user knows
/// the inline-edit affordance exists without a separate footer line.
fn name_hint_line(mut row: Line<'static>) -> Line<'static> {
    row.spans.push(Span::styled(
        "  (e to rename)".to_string(),
        Style::new().fg(Color::DarkGray),
    ));
    row
}

/// The editable session-name row: the label + the buffer with an inverted
/// caret at the cursor (a grapheme under the cursor, or a trailing space
/// block when the cursor is past the end). Matches the input-bar caret style.
fn name_edit_line(field: &crate::input::InputField) -> Line<'static> {
    // Label matches render::field's "{:<22}" so the caret row does not jump
    // left/right against the static rows when editing starts/stops.
    let label: Span<'static> = Span::raw(format!("{:<22}", "Session name:"));
    let body_style = Style::new().fg(Color::Reset);
    let cursor_style = Style::new().bg(Color::White).fg(Color::Black);
    let text = field.value();
    let cursor = field.cursor().min(text.len());
    let before = &text[..cursor];
    let rest = &text[cursor..];
    let mut spans: Vec<Span<'static>> = vec![label];
    if !before.is_empty() {
        spans.push(Span::styled(before.to_string(), body_style));
    }
    if let Some(g) = rest.graphemes(true).next() {
        spans.push(Span::styled(g.to_string(), cursor_style));
        let after = &rest[g.len()..];
        if !after.is_empty() {
            spans.push(Span::styled(after.to_string(), body_style));
        }
    } else {
        spans.push(Span::styled(" ".to_string(), cursor_style));
    }
    spans.push(Span::styled(
        "  (Enter save · Esc cancel)".to_string(),
        Style::new().fg(Color::DarkGray),
    ));
    Line::from(spans)
}

/// The Config tab: runtime configuration knobs -- model, permission mode,
/// sandbox, breaker, + the settings-file memory toggles. Reads live app
/// state + the snapshot's toggle fields so the tab is a focused config view.
/// Display-only: the user flips a toggle by editing the settings file (the
/// settings file is the source of truth, edited externally, not inline).
fn render_config(app: &App) -> String {
    let mode = app.current_mode();
    let snap = app.snapshot_or_stub();
    let f = crate::command::render::field;
    let on_off = |b: bool| if b { "on" } else { "off" };
    let mut s = String::new();
    // Config tab shows the resolved model id (not the tier label) + the
    // applied effort (None = no effort parameter sent, hidden per I8).
    let model_label = app
        .model_catalog
        .active_id
        .as_deref()
        .unwrap_or(&app.status.model);
    s.push_str(&f("Model", model_label));
    if let Some(effort) = app.applied_effort {
        let label = match effort {
            houyicoder_protocol::llm::EffortLevel::Low => "low",
            houyicoder_protocol::llm::EffortLevel::Medium => "medium",
            houyicoder_protocol::llm::EffortLevel::High => "high",
        };
        s.push_str(&f("effort", label));
    }
    s.push_str(&f(
        "Permission mode",
        crate::command::render::permission_mode_label(mode),
    ));
    s.push_str(&f("sandbox", &app.status.sandbox));
    s.push_str(&f(
        "breaker",
        &crate::command::render::render_breaker_line(&snap),
    ));
    s.push_str(&f("auto-memory", on_off(snap.auto_memory)));
    s.push_str(&f("auto-dream", on_off(snap.auto_dream)));
    s.trim_end().to_string()
}

fn render_usage(app: &App) -> String {
    let snap = app.snapshot_or_stub();
    let u = &snap.cumulative_usage;
    let f = crate::command::render::field;
    let ft = crate::command::render::format_tokens;
    let mut s = String::new();
    s.push_str(&f("input", &ft(u.input_tokens as u64)));
    s.push_str(&f("output", &ft(u.output_tokens as u64)));
    // Reasoning tokens: a component of output, shown only when >0. The
    // parenthetical note "(incl. in output)" makes the inclusion relation
    // explicit so it is not read as a separate total (I14).
    if u.reasoning_tokens > 0 {
        s.push_str(&f(
            "reasoning",
            &format!("{} (incl. in output)", ft(u.reasoning_tokens as u64)),
        ));
    }
    s.push_str(&f("cache read", &ft(u.cache_read_input_tokens as u64)));
    s.push_str(&f("cache write", &ft(u.cache_write_input_tokens as u64)));
    s.push_str(&f(
        "tool calls",
        &format!(
            "{} ({} ok / {} err)",
            snap.tool_calls, snap.tool_success, snap.tool_errors
        ),
    ));
    // Per-model breakdown only when two or more models share the session;
    // a single model is already covered by the flat rows above, so a
    // per-model section would just repeat them. Sorted by input+output
    // descending (heaviest first), the model-entries
    // ordering. Reasoning per model only when that model used any.
    if snap.by_model.len() >= 2 {
        s.push_str("Usage by model:\n");
        for m in &snap.by_model {
            let mut row = format!(
                "  {:<16}{} in · {} out",
                format!("{}:", m.model),
                ft(m.input_tokens),
                ft(m.output_tokens),
            );
            if m.reasoning_tokens > 0 {
                row.push_str(&format!(" · {} reasoning", ft(m.reasoning_tokens)));
            }
            row.push_str(&format!(
                " · {} cache rd · {} cache wr",
                ft(m.cache_read_tokens),
                ft(m.cache_write_tokens),
            ));
            s.push_str(&row);
            s.push('\n');
        }
    }
    s.trim_end().to_string()
}

/// The sub-tab header row: Status / Config / Usage, the active one bold cyan,
/// the peers dim. A Settings-modal-style tab title; the active
/// marker echoes the tab highlight.
fn tab_header(active: StatusTab) -> Paragraph<'static> {
    Paragraph::new(tab_header_line(active))
}

/// The Line the tab header renders, extracted so a unit test can assert the
/// three tab titles without poking private widget fields.
fn tab_header_line(active: StatusTab) -> Line<'static> {
    let spans: Vec<Span> = [StatusTab::Status, StatusTab::Config, StatusTab::Usage]
        .iter()
        .flat_map(|t| {
            let is_active = *t == active;
            let style = if is_active {
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            vec![
                Span::styled(format!(" {} ", t.title()), style),
                Span::styled("|", Style::new().fg(Color::DarkGray)),
            ]
        })
        .collect();
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sub-tab header renders Status / Config / Usage, with the active one
    /// marked (the active title appears in the header).
    #[test]
    fn test_header_renders_all_tabs() {
        let line = tab_header_line(StatusTab::Status);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("Status"), "Status tab: {rendered}");
        assert!(rendered.contains("Config"), "Config tab: {rendered}");
        assert!(rendered.contains("Usage"), "Usage tab: {rendered}");
    }

    /// The Config tab renders model / permission mode / sandbox / breaker +
    /// the settings-file memory toggles (auto-memory / auto-dream).
    #[test]
    fn test_config_tab_has_knobs() {
        let mut app = crate::test_support::working_app();
        app.status.model = "qwen3.8-max".into();
        app.status.sandbox = "mac-seatbelt".into();
        let s = render_config(&app);
        assert!(s.contains("qwen3.8-max"), "model: {s}");
        assert!(s.contains("mac-seatbelt"), "sandbox: {s}");
        assert!(s.contains("Permission mode:"), "permission mode: {s}");
        assert!(s.contains("breaker:"), "breaker: {s}");
        assert!(s.contains("auto-memory:"), "auto-memory row: {s}");
        assert!(s.contains("auto-dream:"), "auto-dream row: {s}");
    }

    /// The Config tab renders the toggle state from the snapshot, so a snap
    /// with auto-memory off shows "off" (the row reflects the wire value, not
    /// a hardcoded default).
    #[test]
    fn test_config_tab_reflects_state() {
        let mut app = crate::test_support::working_app();
        let mut snap = app.snapshot_or_stub();
        snap.auto_memory = false;
        snap.auto_dream = true;
        app.status_cache = Some(snap);
        let s = render_config(&app);
        let memory_line = s
            .lines()
            .find(|l| l.trim_start().starts_with("auto-memory"))
            .unwrap_or_else(|| panic!("auto-memory row missing: {s}"));
        let dream_line = s
            .lines()
            .find(|l| l.trim_start().starts_with("auto-dream"))
            .unwrap_or_else(|| panic!("auto-dream row missing: {s}"));
        assert!(memory_line.ends_with("off"), "off toggle: {s}");
        assert!(dream_line.ends_with("on"), "on toggle: {s}");
    }

    /// The Usage tab renders the token counts from the snapshot.
    #[test]
    fn test_usage_tab_has_tokens() {
        let app = crate::test_support::working_app();
        let s = render_usage(&app);
        assert!(s.contains("input:"), "input row: {s}");
        assert!(s.contains("output:"), "output row: {s}");
    }

    /// The Config tab renders the resolved model id + the effort badge
    /// (only when a real effort is applied).
    #[test]
    fn test_config_tab_shows_effort() {
        use houyicoder_protocol::llm::EffortLevel;
        let mut app = crate::test_support::working_app();
        app.status.model = "qwen3.8-max".into();
        app.applied_effort = Some(EffortLevel::High);
        let s = render_config(&app);
        assert!(s.contains("effort:"), "effort row: {s}");
        assert!(s.contains("high"), "high level: {s}");
        // Effort row hidden when None.
        app.applied_effort = None;
        let s = render_config(&app);
        assert!(!s.contains("effort:"), "no effort row when None: {s}");
    }

    /// The Usage tab shows reasoning tokens (incl. in output) only when >0.
    #[test]
    fn test_usage_tab_shows_reasoning() {
        let mut app = crate::test_support::working_app();
        let mut snap = app.snapshot_or_stub();
        snap.cumulative_usage.reasoning_tokens = 1500;
        app.status_cache = Some(snap);
        let s = render_usage(&app);
        assert!(s.contains("reasoning:"), "reasoning row: {s}");
        assert!(s.contains("incl. in output"), "inclusion note: {s}");

        // Hidden when 0.
        let mut app = crate::test_support::working_app();
        let mut snap = app.snapshot_or_stub();
        snap.cumulative_usage.reasoning_tokens = 0;
        app.status_cache = Some(snap);
        let s = render_usage(&app);
        assert!(!s.contains("reasoning:"), "no reasoning when 0: {s}");
    }

    /// The Usage tab renders the per-model section only when two or more
    /// models share the session; a single model is already covered by the
    /// flat rows and a per-model section would just repeat them.
    #[test]
    fn test_single_omits_per_model() {
        let mut app = crate::test_support::working_app();
        let mut snap = app.snapshot_or_stub();
        snap.by_model = vec![houyicoder_protocol::frontend::status::ModelUsageView {
            model: "glm-5.2".into(),
            input_tokens: 1000,
            output_tokens: 500,
            ..Default::default()
        }];
        app.status_cache = Some(snap);
        let s = render_usage(&app);
        assert!(
            !s.contains("Usage by model:"),
            "single model: no per-model section: {s}"
        );
    }

    /// The per-model section lists each model on its own line with the
    /// token counts, sorted heaviest-first, reasoning only when that model
    /// used any. Tokens render compact (k/m) so large counts fit one line.
    #[test]
    fn test_per_model_lists_models() {
        let mut app = crate::test_support::working_app();
        let mut snap = app.snapshot_or_stub();
        snap.by_model = vec![
            houyicoder_protocol::frontend::status::ModelUsageView {
                model: "qwen3.7-max".into(),
                input_tokens: 1_500_000,
                output_tokens: 400_000,
                cache_read_tokens: 1_100_000,
                cache_write_tokens: 900_000,
                reasoning_tokens: 0,
            },
            houyicoder_protocol::frontend::status::ModelUsageView {
                model: "glm-5.2".into(),
                input_tokens: 300_000,
                output_tokens: 100_000,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 8_000,
            },
        ];
        app.status_cache = Some(snap);
        let s = render_usage(&app);
        assert!(s.contains("Usage by model:"), "section header: {s}");
        let lines: Vec<&str> = s.lines().collect();
        // Heaviest (qwen3.7-max, 1.9M in+out) leads glm-5.2 (400k).
        let qwen_idx = lines.iter().position(|l| l.contains("qwen3.7-max"));
        let glm_idx = lines.iter().position(|l| l.contains("glm-5.2"));
        assert!(qwen_idx < glm_idx, "heaviest model leads: {s}");
        let qwen = lines.iter().find(|l| l.contains("qwen3.7-max")).unwrap();
        let glm = lines.iter().find(|l| l.contains("glm-5.2")).unwrap();
        assert!(qwen.contains("1.5m in"), "compact m suffix: {qwen}");
        assert!(qwen.contains("400k out"), "compact k suffix: {qwen}");
        assert!(
            !qwen.contains("reasoning"),
            "qwen reasoning 0 omitted: {qwen}"
        );
        assert!(glm.contains("300k in"), "glm compact: {glm}");
        assert!(glm.contains("8k reasoning"), "glm reasoning shown: {glm}");
    }

    /// Compact formatter: k and m suffixes with trailing .0 trimmed, raw
    /// under 1000. Pins the render the Usage tab depends on.
    #[test]
    fn test_format_tokens_compact() {
        use crate::command::render::format_tokens;
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1k");
        assert_eq!(format_tokens(16100), "16.1k");
        assert_eq!(format_tokens(1_600_000), "1.6m");
    }

    /// The editable name line renders the label + the buffer text (cursor at
    /// the end renders a trailing space block).
    #[test]
    fn test_name_edit_renders_buffer() {
        let mut field = crate::input::InputField::new();
        field.insert_str("fix");
        let line = name_edit_line(&field);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("Session name: "),
            "label present: {rendered}"
        );
        assert!(rendered.contains("fix"), "buffer text present: {rendered}");
    }

    /// An empty buffer still renders the label + a cursor block (the caret at
    /// end), so the user sees where typing lands even before the first char.
    #[test]
    fn test_name_edit_renders_caret() {
        let field = crate::input::InputField::new();
        let line = name_edit_line(&field);
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("Session name: "),
            "label present: {rendered}"
        );
    }

    /// The cursor mid-buffer splits the text: a grapheme under the caret is
    /// styled separately from the before/after text.
    #[test]
    fn test_name_edit_cursor_buffer() {
        let mut field = crate::input::InputField::new();
        field.insert_str("abc");
        field.move_left(); // cursor between b and c (after "ab")
        let line = name_edit_line(&field);
        // Three text spans: "  Session name: ", "ab", the caret char "c"
        // (the cursor sits on 'c' so 'ab' is before + 'c' is the caret).
        let rendered: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(rendered.contains("ab"), "before-caret text: {rendered}");
        assert!(rendered.contains('c'), "caret char: {rendered}");
    }
}
