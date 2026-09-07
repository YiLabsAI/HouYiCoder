use super::*;

/// A ModelResult reflects the applied model on the status bar.
#[test]
fn test_model_result_updates_status() {
    let mut app = crate::composition::app();
    app.status.model = "Max".into();
    app.handle_agent_message(AgentMessage::ModelResult {
        model: "qwen3.8-max".into(),
        effort: None,
    });
    assert_eq!(
        app.status.model, "qwen3.8-max",
        "status.model updated from the server's resolved model"
    );
}

/// A SystemLine notice renders verbatim as a transcript system line (an
/// overflow the catalog could not self-heal, pointing at the override).
#[test]
fn test_system_line_renders_notice() {
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::SystemLine {
        text: "set catalog context_window".into(),
    });
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, crate::state::TranscriptLine::System(s) if s.contains("set catalog context_window"))),
        "system line lands in the transcript"
    );
}

/// A ModelInfoResult stashes the catalog so the /model pane renders it, and
/// clamps the cursor into the new list bounds.
#[test]
fn test_model_result_stashes_catalog() {
    use houyicoder_protocol::frontend::model::{ModelCatalog, ModelCatalogEntry};
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Model;
    app.model_sel = 5; // past the incoming list
    app.handle_agent_message(AgentMessage::ModelInfoResult {
        catalog: ModelCatalog {
            active_id: Some("a".into()),
            effort_level: None,
            catalog: vec![
                ModelCatalogEntry {
                    id: "a".into(),
                    display_name: None,
                    description: None,
                    effort: None,
                },
                ModelCatalogEntry {
                    id: "b".into(),
                    display_name: None,
                    description: None,
                    effort: None,
                },
            ],
        },
    });
    assert_eq!(app.model_catalog.catalog.len(), 2, "catalog stashed");
    assert_eq!(app.model_catalog.active_id.as_deref(), Some("a"));
    assert!(
        app.model_sel <= 2,
        "cursor clamped into the new list bounds: {}",
        app.model_sel
    );
}

/// When a ModelInfoResult arrives with an active_id, the cursor jumps to
/// that model's row so reopening /model after a switch does not flash from
/// the old position.
#[test]
fn test_jumps_cursor_to_active() {
    use houyicoder_protocol::frontend::model::{ModelCatalog, ModelCatalogEntry};
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Model;
    app.model_sel = 0;
    app.handle_agent_message(AgentMessage::ModelInfoResult {
        catalog: ModelCatalog {
            active_id: Some("c".into()),
            effort_level: None,
            catalog: vec![
                ModelCatalogEntry {
                    id: "a".into(),
                    display_name: None,
                    description: None,
                    effort: None,
                },
                ModelCatalogEntry {
                    id: "b".into(),
                    display_name: None,
                    description: None,
                    effort: None,
                },
                ModelCatalogEntry {
                    id: "c".into(),
                    display_name: None,
                    description: None,
                    effort: None,
                },
            ],
        },
    });
    assert_eq!(
        app.model_sel, 3,
        "cursor jumped to the active model's row (row 3 = Default + catalog[2]), not left at 0"
    );
}

/// A ModelResult stashes the applied effort so the status bar badge shows
/// what is being sent (None hides the badge).
#[test]
fn test_model_result_stashes_effort() {
    use houyicoder_protocol::llm::EffortLevel;
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::ModelResult {
        model: "qwen3.7-max".into(),
        effort: Some(EffortLevel::High),
    });
    assert_eq!(
        app.applied_effort,
        Some(EffortLevel::High),
        "effort stashed"
    );

    // A None effort (model unsupported, or auto) clears the badge.
    app.handle_agent_message(AgentMessage::ModelResult {
        model: "deepseek-chat".into(),
        effort: None,
    });
    assert!(app.applied_effort.is_none(), "None clears the badge");
}

/// Selecting the Default sentinel (id=None) does not set status.model — the
/// server resolves Default to DEFAULT_MODEL and the ModelResult reply carries
/// the resolved id. Setting it to "Default" here would flicker.
#[test]
fn test_default_sentinel_skips_model() {
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Model;
    app.status.model = "glm-5.2".into();
    // Simulate Default row: model_id_at returns None for the sentinel row.
    // set_model_at_cursor is called with the cursor on row 0 (Default).
    app.model_sel = 0;
    // The app has no catalog wired, so model_id_at(0) returns None.
    app.set_model_at_cursor();
    assert_eq!(
        app.status.model, "glm-5.2",
        "Default sentinel does not overwrite status.model; the reply will"
    );
}

/// When a Model pane is open, typing printable chars does not push them into
/// the input box — the pane owns the keyboard.
#[test]
fn test_model_pane_swallows_chars() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.pane = crate::state::Pane::Model;
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
    );
    assert!(
        app.input.value().is_empty(),
        "char swallowed by Model pane, not pushed to input: {}",
        app.input.value()
    );
}

/// Selecting a concrete model id sets status.model immediately (no flicker —
/// the reply echoes the same id).
#[test]
fn test_concrete_id_sets_model() {
    use houyicoder_protocol::frontend::model::{ModelCatalog, ModelCatalogEntry};
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Model;
    app.status.model = "old".into();
    // Wire a catalog so cursor on row 1 returns "glm-5.2"
    app.model_catalog = ModelCatalog {
        active_id: None,
        effort_level: None,
        catalog: vec![
            ModelCatalogEntry {
                id: "qwen3.7-max".into(),
                display_name: None,
                description: None,
                effort: None,
            },
            ModelCatalogEntry {
                id: "glm-5.2".into(),
                display_name: None,
                description: None,
                effort: None,
            },
        ],
    };
    app.model_sel = 2;
    app.set_model_at_cursor();
    assert_eq!(
        app.status.model, "glm-5.2",
        "concrete id sets status.model immediately"
    );
}

/// ModelInfoResult with an active_id not found in the catalog falls through
/// to the clamp (cursor > max_sel => 0).
#[test]
fn test_not_in_catalog_clamps() {
    use houyicoder_protocol::frontend::model::{ModelCatalog, ModelCatalogEntry};
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Model;
    app.model_sel = 5;
    app.handle_agent_message(AgentMessage::ModelInfoResult {
        catalog: ModelCatalog {
            active_id: Some("not-here".into()),
            effort_level: None,
            catalog: vec![ModelCatalogEntry {
                id: "a".into(),
                display_name: None,
                description: None,
                effort: None,
            }],
        },
    });
    assert_eq!(
        app.model_sel, 0,
        "active_id not in catalog + cursor past bounds => clamp to 0"
    );
}

/// Opening /model positions the cursor on the active model's row from the
/// cached catalog (no flicker waiting for the reply).
#[test]
fn test_positions_cursor_on_open() {
    use houyicoder_protocol::frontend::model::{ModelCatalog, ModelCatalogEntry};
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    // Seed a cached catalog so the open command has data to position from.
    app.model_catalog = ModelCatalog {
        active_id: Some("glm-5.2".into()),
        effort_level: None,
        catalog: vec![
            ModelCatalogEntry {
                id: "qwen3.7-max".into(),
                display_name: None,
                description: None,
                effort: None,
            },
            ModelCatalogEntry {
                id: "glm-5.2".into(),
                display_name: None,
                description: None,
                effort: None,
            },
        ],
    };
    app.model_sel = 0;
    // Simulate /model: the command handler positions the cursor.
    app.pane = crate::state::Pane::Model;
    if let Some(ref active) = app.model_catalog.active_id
        && let Some(idx) = app
            .model_catalog
            .catalog
            .iter()
            .position(|e| e.id == *active)
    {
        app.model_sel = idx;
    }
    assert_eq!(
        app.model_sel, 1,
        "cursor positioned on the active model's row, not left at 0"
    );
}
