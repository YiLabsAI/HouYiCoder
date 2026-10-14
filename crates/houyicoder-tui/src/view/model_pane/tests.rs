use super::*;
use crate::session::capabilities::model::PendingCommit;
use crate::state::Pane;
use crate::test_harness::{model_app, model_caps, model_catalog, model_entry, render_text};
use houyicoder_protocol::envelope::RequestId;
use houyicoder_protocol::frontend::model::{
    FastModeAvailability, ModelDisplayCapabilities, SpeedMode,
};

/// The catalog every layout test draws: distinct ids and names, all
/// advertising the same window so a row's right column is checkable.
const ROWS: [(&str, &str); 5] = [
    ("model-one", "One"),
    ("model-two", "Two"),
    ("model-three", "Three"),
    ("model-four", "Four"),
    ("model-five", "Five"),
];

/// An App showing /model over the given rows, each advertising the same
/// window; a terminal width of w leaves the pane an inner width of w-4.
fn pane_app(rows: &[(&str, &str)]) -> App {
    let mut app = model_app(model_catalog(
        rows.iter()
            .map(|(id, name)| model_entry(id, name, model_caps(true, false)))
            .collect(),
    ));
    app.ui.navigation.pane = Pane::Model;
    app
}

/// The rendered line carrying the focused model's window, which only the
/// narrow layout's fixed detail row carries.
fn detail_line(out: &str) -> &str {
    out.lines()
        .find(|line| line.contains("1M context"))
        .unwrap_or_else(|| panic!("the detail row renders: {out}"))
}

fn line_index(out: &str, needle: &str) -> usize {
    out.lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle} renders: {out}"))
}

/// A Cooldown availability renders its reason and never takes the
/// setting focus, the same as an unavailable tier: the pane shows why,
/// the arrows skip it.
#[test]
fn test_picker_cooldown_fast() {
    let mut app = model_app(model_catalog(vec![model_entry(
        "model-one",
        "One",
        ModelDisplayCapabilities {
            fast: FastModeAvailability::Cooldown {
                reason: "rate limited until 12:00".into(),
                reset_at_ms: 1_000,
            },
            ..model_caps(true, false)
        },
    )]));
    app.ui.navigation.pane = Pane::Model;
    let out = render_text(&app, WIDE_INNER_WIDTH + 4, 24);
    assert!(
        out.contains("unavailable (rate limited until 12:00)"),
        "the cooldown reason renders: {out}"
    );
    app.cycle_model_setting();
    assert_eq!(
        app.ui.model.focus,
        ModelSettingFocus::Effort,
        "a cooldown tier is not focusable"
    );
}

/// The empty-catalog footer stays short so a narrow width keeps the Esc
/// hint; the configuration guidance lives in the list body.
#[test]
fn test_picker_empty_footer_short() {
    let mut app = model_app(model_catalog(Vec::new()));
    app.ui.navigation.pane = Pane::Model;
    let out = render_text(&app, 40, 24);
    assert!(
        out.contains("no catalog configured"),
        "the guidance renders in the body: {out}"
    );
    let footer = out
        .lines()
        .find(|l| l.contains("No models configured"))
        .unwrap_or_else(|| panic!("the empty footer renders: {out}"));
    assert!(
        footer.contains("Esc to cancel"),
        "the Esc hint survives a narrow width: {footer}"
    );
}

/// The layout switches on the pane's own inner width, not on the terminal
/// width: a terminal one column either side of the threshold picks the
/// other layout. The wide side puts each row's window on its row; the
/// narrow side keeps one window line, the focused row's.
#[test]
fn test_picker_layout_boundary() {
    let app = pane_app(&ROWS[..2]);
    let wide = render_text(&app, WIDE_INNER_WIDTH + 4, 24);
    assert_eq!(
        wide.lines().filter(|l| l.contains("1M context")).count(),
        3,
        "at the threshold inner width every row carries its window: {wide}"
    );
    let narrow = render_text(&app, WIDE_INNER_WIDTH + 3, 24);
    assert_eq!(
        narrow.lines().filter(|l| l.contains("1M context")).count(),
        1,
        "one column under the threshold the window moves to the detail row: {narrow}"
    );
}

/// M-30: at a wide inner width every row carries its name, the id the
/// provider sees and its window on one line, so the catalog can be scanned
/// without moving the cursor.
#[test]
fn test_picker_wide_rows() {
    let app = pane_app(&ROWS);
    let out = render_text(&app, 84, 24);
    for (id, name) in ROWS {
        let line = out
            .lines()
            .find(|line| line.contains(id))
            .unwrap_or_else(|| panic!("the row for {id} renders: {out}"));
        assert!(
            line.contains(name) && line.contains("1M context"),
            "one line carries the name, the id and the window: {line}"
        );
    }
    assert_eq!(
        out.lines()
            .filter(|line| line.contains("1M context"))
            .count(),
        ROWS.len() + 1,
        "one line per catalog row plus the Default row: {out}"
    );
}

/// M-31: at a narrow inner width each model keeps one line and the focused
/// model's id and window move to a fixed detail row. The detail row and the
/// settings keep their lines as the focus moves, and the id and window
/// survive the narrowest width.
#[test]
fn test_picker_narrow_detail() {
    let mut app = pane_app(&ROWS[..2]);
    // The reseed already puts the cursor on the session's model (row 1).
    let first = render_text(&app, 64, 24);
    let detail = detail_line(&first);
    assert!(
        detail.contains("model-one"),
        "the detail names the focused model's id: {detail}"
    );
    assert_eq!(
        first
            .lines()
            .filter(|line| line.contains("1M context"))
            .count(),
        1,
        "the window lives in the fixed detail row, not per row: {first}"
    );
    let effort_row = line_index(&first, "Reasoning Effort:");
    let detail_row = line_index(&first, "1M context");

    app.move_model_focus(1);
    let second = render_text(&app, 64, 24);
    assert!(
        detail_line(&second).contains("model-two"),
        "the detail swaps in place: {}",
        detail_line(&second)
    );
    assert_eq!(
        line_index(&second, "Reasoning Effort:"),
        effort_row,
        "the settings keep their line as the focus moves"
    );
    assert_eq!(
        line_index(&second, "1M context"),
        detail_row,
        "the detail keeps its line as the focus moves"
    );
    let squeezed = render_text(&app, 44, 24);
    let detail = detail_line(&squeezed);
    assert!(
        detail.contains("model-two") && detail.contains("1M context"),
        "the id and the window survive the narrowest width: {detail}"
    );
    assert!(
        !detail.contains("from model table"),
        "the provenance goes before the word that names the window: {detail}"
    );
}

/// A 40-column pane still shows the commit and cancel keys: the narrow
/// footer cuts the navigation hints, never the two keys that close it.
#[test]
fn test_picker_narrow_footer_keys() {
    let app = pane_app(&ROWS);
    let narrow = render_text(&app, 40, 24);
    assert!(
        narrow
            .lines()
            .any(|l| l.contains("Enter to save") && l.contains("Esc to cancel")),
        "the narrow footer keeps both closing keys: {narrow}"
    );
    let wide = render_text(&app, WIDE_INNER_WIDTH + 4, 24);
    assert!(
        wide.lines().any(|l| l.contains("Tab to setting")),
        "the wide footer keeps the navigation hints: {wide}"
    );
}

/// While a commit is with the host the footer reports the in-flight save
/// instead of promising keys that are held.
#[test]
fn test_picker_pending_footer_saving() {
    let mut app = pane_app(&ROWS);
    app.sessions.current.capabilities.model.pending = Some(PendingCommit {
        req_id: RequestId(1),
        prior_speed: SpeedMode::Standard,
    });
    let out = render_text(&app, WIDE_INNER_WIDTH + 4, 24);
    assert!(
        out.lines().any(|l| l.contains("saving")),
        "the footer reports the in-flight save: {out}"
    );
    assert!(
        !out.lines().any(|l| l.contains("Enter to save")),
        "held keys are not promised while pending: {out}"
    );
}

/// Auto is the chain's value when nothing is persisted: the level reads as
/// the default rather than as an adjustment already made.
#[test]
fn test_picker_auto_default_marker() {
    let mut app = model_app(model_catalog(vec![model_entry(
        "model-one",
        "One",
        model_caps(true, true),
    )]));
    app.ui.navigation.pane = Pane::Model;
    let out = render_text(&app, 84, 24);
    assert!(
        out.contains("auto (default)"),
        "an unpersisted effort reads auto, marked the default: {out}"
    );
}

/// A model the catalog declares no fast tier for drops the Fast Mode row
/// and the Tab hint entirely rather than printing an unavailable line.
#[test]
fn test_picker_hides_fast_unconfigured() {
    let mut caps = model_caps(true, true);
    caps.fast = FastModeAvailability::NotConfigured;
    let mut app = model_app(model_catalog(vec![model_entry("model-one", "One", caps)]));
    app.ui.navigation.pane = Pane::Model;
    let wide = render_text(&app, WIDE_INNER_WIDTH + 4, 24);
    assert!(
        !wide.contains("Fast Mode"),
        "an unconfigured tier renders no Fast Mode row: {wide}"
    );
    assert!(
        !wide.lines().any(|l| l.contains("Tab to setting")),
        "an unconfigured tier drops the Tab hint: {wide}"
    );
}
