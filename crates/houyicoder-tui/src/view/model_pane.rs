//! /model pane content: renders a Default sentinel row + the catalog rows
//! from settings.json into the shared Pane template. The active id renders
//! with a check mark; the cursor row is highlighted. Up / Down navigate,
//! Enter selects (Default sends the sentinel; a catalog row sends that id),
//! Esc closes. The catalog arrives over the wire (ModelInfoResult), so the
//! rows reflect settings.json, not a hardcoded list.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph},
};

use crate::state::App;
use houyicoder_protocol::frontend::model::ModelCatalogEntry;

/// Default height /model asks for: a header + the list + a footer. Capped at
/// half the main area by draw_command_pane.
pub(crate) const MODEL_PANE_HEIGHT: u16 = 12;

/// The number of rows the pane lists: the Default sentinel (always present)
/// plus the catalog entries. Used by Up/Down to clamp the cursor.
pub fn model_row_count(app: &App) -> usize {
    app.model_catalog.catalog.len() + 1
}

/// The id to send for a given row index, or None for the Default sentinel.
/// Index 0 is Default; index i>=1 maps to catalog[i-1]. None for an
/// out-of-range row (clamped by the caller).
pub fn model_id_at(app: &App, idx: usize) -> Option<String> {
    if idx == 0 {
        None
    } else {
        app.model_catalog.catalog.get(idx - 1).map(|e| e.id.clone())
    }
}

/// The row index for a given model id, or 0 (Default) when the id is None
/// or not found in the catalog. The inverse of model_id_at: row 0 is the
/// Default sentinel, row i+1 is catalog[i]. Callers that position the
/// cursor from a catalog index must add 1 to account for the Default row
/// — this helper is the single point that owns that +1 so the two spaces
/// (row index vs catalog index) never get conflated.
pub fn row_for_model_id(app: &App, id: Option<&str>) -> usize {
    match id {
        None => 0,
        Some(id) => app
            .model_catalog
            .catalog
            .iter()
            .position(|e| e.id == id)
            .map(|idx| idx + 1)
            .unwrap_or(0),
    }
}

/// Render the /model content into the Pane inner rect. Header + list + footer.
pub(crate) fn draw_content(f: &mut Frame, inner: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            "Select a model",
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        )])),
        chunks[0],
    );
    let active_id = app.model_catalog.active_id.as_deref();
    let items: Vec<ListItem> = std::iter::once(default_row(active_id))
        .chain(
            app.model_catalog
                .catalog
                .iter()
                .enumerate()
                .map(|(i, e)| catalog_row(i, e, active_id)),
        )
        .collect();
    let mut state = ListState::default();
    state.select(Some(
        app.model_sel.min(model_row_count(app).saturating_sub(1)),
    ));
    let list = List::new(items)
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(list, chunks[1], &mut state);
    // Effort selector row: three levels + "default: X" + ←/→ hint, or
    // "not supported" when the focused model speaks no effort dialect.
    let focused_id = model_id_at(app, app.model_sel);
    let effort_line = effort_selector_line(&focused_id, app);
    f.render_widget(Paragraph::new(effort_line), chunks[2]);
    let footer_text = if app.model_catalog.catalog.is_empty() {
        "no catalog configured; add model.catalog entries to settings.json · Esc=close"
    } else {
        "Enter=save · Esc=close"
    };
    let footer = Paragraph::new(footer_text).style(Style::new().fg(Color::DarkGray));
    f.render_widget(footer, chunks[3]);
}

/// Build the effort selector line for the focused model. Shows the three
/// levels (low/medium/high) with the current pick highlighted + "default: X"
/// when the pick matches the dialect's default, + a ←/→ hint. Renders "not
/// supported" when the focused model speaks no effort dialect.
fn effort_selector_line(focused_id: &Option<String>, app: &App) -> Line<'static> {
    let model = focused_id
        .as_deref()
        .or(app.model_catalog.active_id.as_deref())
        .unwrap_or("");
    let dim = Style::new().fg(Color::DarkGray);
    let bold = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    if !supports_effort(model) {
        return Line::from(vec![
            Span::styled("  ", dim),
            Span::styled("Effort not supported", dim),
        ]);
    }
    let levels = [
        houyicoder_protocol::llm::EffortLevel::Low,
        houyicoder_protocol::llm::EffortLevel::Medium,
        houyicoder_protocol::llm::EffortLevel::High,
    ];
    let labels = ["low", "medium", "high"];
    let current = app.model_effort;
    let mut spans = vec![Span::raw("  ")];
    for (i, (level, label)) in levels.iter().zip(labels.iter()).enumerate() {
        if i > 0 {
            spans.push(Span::raw(" / "));
        }
        let is_current = current == Some(*level);
        spans.push(Span::styled(
            if is_current {
                format!("[{label}]")
            } else {
                format!(" {label} ")
            },
            if is_current { bold } else { dim },
        ));
    }
    spans.push(Span::raw("  ← → to adjust"));
    Line::from(spans)
}

/// Whether a model id speaks an effort dialect (qwen3 / o1·o3·gpt-5). Matches
/// the core + provider substring probes — TUI cannot depend on either crate,
/// so the probe lives here. Not a validity check: a typo still matches, an
/// unlisted model still runs.
pub fn supports_effort(model: &str) -> bool {
    let m = model.to_lowercase();
    m.contains("qwen3") || m.contains("o1") || m.contains("o3") || m.contains("gpt-5")
}

/// The Default sentinel row. Marked active when no concrete id is set.
fn default_row(active_id: Option<&str>) -> ListItem<'static> {
    let is_active = active_id.is_none();
    let desc = active_id
        .map(|id| format!("use the default model (currently {id})"))
        .unwrap_or_else(|| "use the default model".to_string());
    ListItem::new(format_row_line(0, "Default", &desc, is_active))
}

/// One catalog row. Marked active when its id matches the active id.
fn catalog_row(
    idx: usize,
    entry: &ModelCatalogEntry,
    active_id: Option<&str>,
) -> ListItem<'static> {
    let name = entry.display_name.as_deref().unwrap_or(&entry.id);
    let desc = entry.description.as_deref().unwrap_or("");
    let is_active = active_id == Some(entry.id.as_str());
    ListItem::new(format_row_line(idx + 1, name, desc, is_active))
}

/// One row's Line: number + name (+ ✔ when active) + a dim description.
fn format_row_line(idx: usize, name: &str, desc: &str, is_active: bool) -> Line<'static> {
    let check = if is_active { " ✔" } else { "  " };
    Line::from(vec![
        Span::raw(format!("  {}.", idx + 1)),
        Span::styled(
            format!(" {name}{check}"),
            Style::new()
                .fg(if is_active { Color::Cyan } else { Color::White })
                .add_modifier(if is_active {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ),
        Span::styled(format!("  {desc}"), Style::new().fg(Color::DarkGray)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(line: Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// The Default row is active when no concrete id is set (the sentinel).
    #[test]
    fn test_default_active_no_id() {
        let is_active = Option::<&str>::None.is_none();
        let line = format_row_line(0, "Default", "use the default model", is_active);
        assert!(rendered(line).contains("✔"), "Default active when no id");
    }

    /// The Default row is inactive when a concrete id is set.
    #[test]
    fn test_default_inactive_id_set() {
        let is_active = Option::<&str>::Some("qwen3.7-max").is_none();
        let line = format_row_line(
            0,
            "Default",
            "use the default model (currently qwen3.7-max)",
            is_active,
        );
        assert!(
            !rendered(line).contains("✔"),
            "Default inactive when id set"
        );
    }

    /// A catalog row renders its display name + the check on id match.
    #[test]
    fn test_catalog_row_renders_name() {
        let line = format_row_line(1, "Max", "most capable", true);
        let r = rendered(line);
        assert!(r.contains("✔"), "active catalog row shows check");
        assert!(r.contains("Max"), "display name rendered");

        let line = format_row_line(1, "Max", "most capable", false);
        assert!(
            !rendered(line).contains("✔"),
            "inactive catalog row no check"
        );
    }

    /// The effort selector renders three levels + ←/→ hint for a supported
    /// model, and "not supported" for an unsupported one.
    #[test]
    fn test_effort_selector_renders_levels() {
        let mut app = crate::composition::app();
        app.model_effort = Some(houyicoder_protocol::llm::EffortLevel::High);
        let line = effort_selector_line(&Some("qwen3.7-max".into()), &app);
        let r = rendered(line);
        assert!(r.contains("high"), "current level shown");
        assert!(r.contains("low"), "low level listed");
        assert!(r.contains("medium"), "medium level listed");
        assert!(r.contains("← → to adjust"), "arrow hint shown");
    }

    #[test]
    fn test_effort_selector_not_supported() {
        let app = crate::composition::app();
        let line = effort_selector_line(&Some("deepseek-chat".into()), &app);
        let r = rendered(line);
        assert!(r.contains("not supported"), "not supported shown");
        assert!(!r.contains("low"), "no levels for unsupported");
    }

    /// supports_effort matches qwen3 / o1 / o3 / gpt-5; misses deepseek/glm.
    #[test]
    fn test_supports_effort_matches_families() {
        assert!(supports_effort("qwen3.7-max"));
        assert!(supports_effort("QWEN3-CODER"));
        assert!(supports_effort("o3-mini"));
        assert!(supports_effort("gpt-5"));
        assert!(!supports_effort("deepseek-chat"));
        assert!(!supports_effort("glm-5.2"));
        assert!(!supports_effort(""));
    }

    fn app_with_catalog(ids: &[&str], active: Option<&str>) -> App {
        let mut app = crate::composition::app();
        app.model_catalog.catalog = ids
            .iter()
            .map(|id| ModelCatalogEntry {
                id: (*id).into(),
                display_name: Some((*id).into()),
                description: None,
                effort: None,
            })
            .collect();
        app.model_catalog.active_id = active.map(|s| s.to_string());
        app
    }

    /// row_for_model_id is the inverse of model_id_at: row 0 is Default,
    /// row i+1 is catalog[i]. None or not-found maps to 0 (Default).
    #[test]
    fn test_row_for_model_inverse() {
        let app = app_with_catalog(&["fable", "max", "mini"], Some("max"));
        assert_eq!(row_for_model_id(&app, None), 0, "None -> Default row 0");
        assert_eq!(
            row_for_model_id(&app, Some("fable")),
            1,
            "catalog[0] -> row 1"
        );
        assert_eq!(
            row_for_model_id(&app, Some("max")),
            2,
            "catalog[1] -> row 2"
        );
        assert_eq!(
            row_for_model_id(&app, Some("mini")),
            3,
            "catalog[2] -> row 3"
        );
        assert_eq!(
            row_for_model_id(&app, Some("nonexistent")),
            0,
            "not found -> Default row 0"
        );
        // Round-trip: model_id_at(row_for_model_id(id)) == id
        for id in &["fable", "max", "mini"] {
            let row = row_for_model_id(&app, Some(id));
            let back = model_id_at(&app, row);
            assert_eq!(back.as_deref(), Some(*id), "round-trip {id}");
        }
        // Default round-trips to None
        let row = row_for_model_id(&app, None);
        assert_eq!(model_id_at(&app, row), None, "Default round-trips to None");
    }

    /// The cursor lands on the active model's row, not one row above it.
    /// This is the bug: catalog index was used as the row index, missing
    /// the +1 for the Default sentinel. With the active model at
    /// catalog[1], the cursor must be on row 2 (the model's row), not
    /// row 1 (the previous model's row).
    #[test]
    fn test_cursor_on_active_row() {
        let mut app = app_with_catalog(&["fable", "max", "mini"], Some("max"));
        // Simulate the command.rs positioning path
        if let Some(ref active) = app.model_catalog.active_id {
            app.model_sel = row_for_model_id(&app, Some(active));
        }
        assert_eq!(
            app.model_sel, 2,
            "active model at catalog[1] -> row 2, not row 1"
        );
        // The id at the cursor row must be the active model's id
        let id_at_cursor = model_id_at(&app, app.model_sel);
        assert_eq!(
            id_at_cursor.as_deref(),
            Some("max"),
            "cursor row resolves to the active model"
        );
    }
}
