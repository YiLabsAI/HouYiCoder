use super::*;

use houyicoder_protocol::frontend::model::{AppliedModel, SpeedMode};
use houyicoder_protocol::frontend::status::StatusSnapshot;
use houyicoder_protocol::llm::{EffortLevel, Usage};
use ratatui::{Terminal, backend::TestBackend};

use crate::agent_message::FleetEntry;
use crate::composition::app;
use crate::state::App;
use crate::state::Screen;
use crate::test_harness::{connected_app, render_text};
use std::time::Instant;

#[test]
fn test_tiny_mark_two_spans() {
    assert_eq!(logo::tiny().len(), 2);
}

fn fleet_app(n: usize, granted: u16) -> App {
    let mut app = app();
    for i in 0..n {
        app.sessions
            .current
            .capabilities
            .fleet
            .entries
            .push(FleetEntry {
                agent_id: format!("c{i}"),
                subagent_type: "explore".into(),
                turn: 1,
                tokens: 10,
                tool_uses: 0,
                last_activity: None,
                completed: None,
                completed_at: None,
                started_at: None,
            });
    }
    app.ui.frame.layout.fleet_granted.set(granted);
    app
}

/// The hint follows what the strip can honor: rows visible -> per-row
/// keys; summary -> the pane; strip dropped -> the pane, tersely. Empty
/// fleet -> no hint at all, so nothing lingers after the grace window
/// drops the entries.
#[test]
fn test_agents_hint_ladder() {
    assert_eq!(agents_hint(&fleet_app(0, 0)), None);
    assert_eq!(
        agents_hint(&fleet_app(2, 2)).unwrap(),
        "2 agents · shift+\u{2191}\u{2193} · enter"
    );
    assert_eq!(
        agents_hint(&fleet_app(1, 1)).unwrap(),
        "1 agent · shift+\u{2191}\u{2193} · enter",
        "a single agent on one row is a real row, not a summary"
    );
    assert_eq!(
        agents_hint(&fleet_app(3, 1)).unwrap(),
        "3 agents · /agents to manage"
    );
    assert_eq!(agents_hint(&fleet_app(3, 0)).unwrap(), "3 agents · /agents");
}

/// The draw publishes the footer budget's grant, so the hint describes
/// the strip the frame actually rendered. The ladder above pokes the
/// value directly, which cannot catch a draw that stopped writing it.
#[test]
fn test_hint_follows_drawn_grant() {
    // Seeded with the strip dropped; only the draw can raise it. The
    // hint needs the connected agent bar, so this is the connected
    // working screen rather than the bare fixture.
    let mut app = connected_app();
    app.ui.navigation.screen = Screen::Working;
    for i in 0..2 {
        app.sessions
            .current
            .capabilities
            .fleet
            .entries
            .push(FleetEntry {
                agent_id: format!("c{i}"),
                subagent_type: "explore".into(),
                turn: 1,
                tokens: 10,
                tool_uses: 0,
                last_activity: None,
                completed: None,
                completed_at: None,
                started_at: None,
            });
    }
    app.ui.frame.layout.fleet_granted.set(0);

    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("2 agents \u{b7} shift+"),
        "two live children on drawn rows advertise the per-row keys:\n{out}"
    );
}

/// A completed child still in the grace window is excluded from the
/// hint count — the strip keeps the row briefly so the user sees the
/// result, but the status bar must not call a finished delegation live.
#[test]
fn test_agents_hint_excludes_completed() {
    let mut app = fleet_app(2, 2);
    let done = FleetEntry {
        agent_id: "done".into(),
        subagent_type: "explore".into(),
        turn: 1,
        tokens: 10,
        tool_uses: 0,
        last_activity: None,
        completed: Some("completed".into()),
        completed_at: Some(Instant::now()),
        started_at: None,
    };
    app.sessions.current.capabilities.fleet.entries.push(done);
    let hint = agents_hint(&app).unwrap();
    assert!(hint.contains("2 agents"), "completed excluded: {hint}");
    assert!(!hint.contains("3 agent"), "no false live count: {hint}");
}

#[test]
fn test_gauge_label_hint() {
    // Below 90%: plain "context used", no /compact hint.
    assert!(
        context_gauge_label(50.0).contains("context used"),
        "no hint below 90: {}",
        context_gauge_label(50.0)
    );
    assert!(
        !context_gauge_label(50.0).contains("/compact"),
        "no /compact hint below 90"
    );
    // At/above 90% (red zone): the /compact hint appears so the action is
    // one glance away.
    let high = context_gauge_label(92.0);
    assert!(high.contains("/compact"), "hint appears at 90+: {high}");
    assert!(high.contains("92"), "percent shown: {high}");
}

#[test]
fn test_progress_str_contains_all() {
    let app = app();
    let s = progress_str(&app);
    assert!(s.contains("design"), "got [{s}]");
    assert!(s.contains("implement"), "got [{s}]");
    assert!(s.contains("verify"), "got [{s}]");
}

#[test]
fn test_for_stage_maps_focus() {
    assert_eq!(
        ViewportMode::for_stage(Stage::Implementing),
        ViewportMode::Focus
    );
    assert_eq!(ViewportMode::for_stage(Stage::Verify), ViewportMode::Focus);
    assert_eq!(ViewportMode::for_stage(Stage::Idle), ViewportMode::Working);
    assert_eq!(
        ViewportMode::for_stage(Stage::Design),
        ViewportMode::Working
    );
    assert_eq!(ViewportMode::for_stage(Stage::Done), ViewportMode::Working);
}

/// The status bar shows the active model id so the user knows which
/// model the next request uses. Effort lives in the /model pane and the
/// Config status tab, not duplicated on the bar.
#[test]
fn test_bar_shows_model_id() {
    let mut app = app();
    app.sessions.current.capabilities.model.catalog.applied = AppliedModel {
        id: "qwen3.7-max".into(),
        effort: Some(EffortLevel::High),
        speed: SpeedMode::Standard,
    };
    let backend = TestBackend::new(80, 3);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw_agent_status_bar(f, f.area(), &app))
        .unwrap();
    let text: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("qwen3.7-max"), "model id shown: {text}");

    // No effort parameter sent -> the whole segment goes.
    app.sessions
        .current
        .capabilities
        .model
        .catalog
        .applied
        .effort = None;
    let backend = TestBackend::new(80, 3);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw_agent_status_bar(f, f.area(), &app))
        .unwrap();
    let text: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(!text.contains("effort"), "no effort segment: {text}");
    assert!(text.contains("qwen3.7-max"), "model id still shown: {text}");
}

#[test]
fn test_bar_drops_token_noise() {
    // A run reported usage (in + out + cache read). The bar must not
    // surface token arrows or a cache pct: that is noise /context and
    // /trajectory already carry. This exercises the post-run snapshot
    // the pre-run render test never reaches, so the noise path stays
    // guarded against re-introduction.
    let mut app = app();
    app.sessions.current.capabilities.model.catalog.applied.id = "test-model".into();
    app.sessions.current.capabilities.status.status_cache = Some(StatusSnapshot {
        model: "test-model".into(),
        breaker_state: None,
        cumulative_usage: Usage {
            input_tokens: 16100,
            output_tokens: 4200,
            cache_read_input_tokens: 12000,
            ..Default::default()
        },
        last_input_tokens: 16100,
        context_used_tokens: Some(16100),
        context_window: 200_000,
        ..Default::default()
    });
    let backend = TestBackend::new(80, 3);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw_agent_status_bar(f, f.area(), &app))
        .unwrap();
    let text: String = term
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    for noise in ["↑", "↓", "cache ", "16.1k", "4.2k"] {
        assert!(
            !text.contains(noise),
            "token noise {noise:?} leaked into bar: {text}"
        );
    }
    // The model + context gauge (the kept reads) still appear.
    assert!(text.contains("test-model"), "model id shown: {text}");
    assert!(
        text.contains("context") || text.contains("%"),
        "context gauge missing: {text}"
    );
}

#[test]
fn test_bar_gauge_needs_measurement() {
    // Before a turn has assembled a context there is no occupancy to
    // report. The gauge stays hidden rather than claiming the window is
    // a known amount full.
    let mut app = app();
    app.sessions.current.capabilities.model.catalog.applied.id = "test-model".into();
    for width in [48, 80] {
        let backend = TestBackend::new(width, 3);
        let mut term = Terminal::new(backend).unwrap();
        for (used, expected) in [
            (None, None),
            (Some(0), Some("0% context used")),
            (Some(16000), Some("8% context used")),
            (Some(180000), Some("90% used · /compact")),
            (None, None),
        ] {
            app.sessions.current.capabilities.status.status_cache = Some(StatusSnapshot {
                model: "test-model".into(),
                context_used_tokens: used,
                context_window: 200_000,
                ..Default::default()
            });
            term.draw(|f| draw_agent_status_bar(f, f.area(), &app))
                .unwrap();
            let text: String = term
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("test-model"), "model id shown: {text}");
            match expected {
                Some(label) => assert!(text.contains(label), "{label}: {text}"),
                None => {
                    assert!(!text.contains("context"), "unmeasured: {text}");
                    assert!(!text.contains('%'), "unmeasured: {text}");
                    assert!(!text.contains("/compact"), "unmeasured: {text}");
                }
            }
        }
    }
}
