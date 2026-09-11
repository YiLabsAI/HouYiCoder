//! Slash-command routing and rendered command-surface tests.

#![cfg(test)]

use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::model::{ModelCatalog, ModelCatalogEntry};

use crate::composition;
use crate::state::{Pane, Screen, Stage, Verdict};
use crate::test_support::render_text;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.agent_directory = Some("## Available agents\n\n- explore: fast read-only search".into());
    app
}

fn render(app: &crate::state::App) -> String {
    render_text(app, 100, 28)
}

#[test]
fn test_palette_inline_filters() {
    let mut app = working();
    let closed = render(&app);
    app.open_palette();
    let open = render(&app);
    println!("--- palette open (inline) ---\n{open}\n--- end ---");
    assert!(open.contains("/ commands"), "palette title missing");
    assert!(open.contains("filter:"), "palette filter line missing");
    // palette is inline: the input placeholder still renders below it.
    assert!(
        open.contains("let's build"),
        "input placeholder should show below palette"
    );
    // the closed render had no palette title
    assert!(!closed.contains("/ commands"));
    // type to filter
    app.palette_push('v');
    app.palette_push('e');
    let filtered = render(&app);
    println!("--- palette filtered 've' ---\n{filtered}\n--- end ---");
    let cmd = app.selected_command().expect("non-empty");
    assert!(cmd.name().contains("ve"));
}

#[test]
fn test_tab_cycle_no_pollution() {
    // Shift+Tab ships a PermissionCycleMode wire verb; it must NOT push a
    // system line — the status-bar pill is the single source of mode truth,
    // so the chat surface stays clean (the cycle itself lands server-side;
    // the pill flips when the PermissionMode reply arrives).
    let mut app = working();
    let before = app.transcript.len();
    app.tab_cycle_mode();
    assert_eq!(
        app.transcript.len(),
        before,
        "Shift+Tab must not pollute the chat surface"
    );
}

#[test]
fn test_tab_cycles_panes() {
    let mut app = working();
    for p in Pane::CYCLE {
        app.pane = p;
        let out = render(&app);
        println!("--- pane {p:?} ---\n{out}\n--- end ---");
        // each pane renders without panic; its identity appears somewhere in
        // the render. Transcript is borderless by design (no title label);
        // with an empty transcript the input placeholder carries the hint.
        if matches!(p, Pane::Transcript) {
            assert!(out.contains("let's build"), "placeholder missing");
        } else {
            assert!(out.contains(p.label()), "pane {p:?} label missing");
        }
    }
}

#[test]
fn test_progress_bar_updates() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    let out = render(&app);
    println!("--- strip at design ---\n{out}\n--- end ---");
    // progress bar shows the three stages with current/pending marks
    assert!(out.contains("design"), "design stage missing");
    assert!(out.contains("implement"), "implement stage missing");
    assert!(out.contains("verify"), "verify stage missing");
    app.approve_in_pane(); // -> implement
    let out = render(&app);
    println!("--- strip at implement ---\n{out}\n--- end ---");
    // design is now done (check mark), implement is current
    assert!(out.contains('\u{2713}'), "done stage should show a check");
}

#[test]
fn test_clear_resets_full_chain() {
    let mut app = working();
    // dirty the state
    app.run_command(SlashCommand::Implement);
    app.approve_in_pane();
    app.run_command(SlashCommand::Clear);
    let out = render(&app);
    println!("--- after /clear ---\n{out}\n--- end ---");
    assert_eq!(app.stage, Stage::Idle);
    assert_eq!(app.pane, Pane::Transcript);
    assert!(app.spec_ctx.step == "idle");
    assert!(
        app.diff
            .hunks
            .iter()
            .all(|h| h.approved == Verdict::Pending)
    );
    assert!(
        app.review
            .findings
            .iter()
            .all(|f| f.signoff == Verdict::Pending),
        "findings should reset to pending"
    );
    assert!(
        app.review.audit_trail.is_empty(),
        "audit trail should clear"
    );
    assert!(out.contains("archived"), "archive system line missing");
    assert_eq!(app.transcript.len(), 1);
}

#[test]
fn test_rewind_pops_last_stage() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // design -> implement
    app.run_command(SlashCommand::Rewind);
    let out = render(&app);
    println!("--- after /rewind ---\n{out}\n--- end ---");
    assert_eq!(app.stage, Stage::Design, "rewind should restore design");
    assert!(out.contains("rewound"), "rewound system line missing");
}

#[test]
fn test_resume_reports_no_store() {
    // Without a session lister, resume reports that the store is unavailable.
    let mut app = working();
    app.run_command(SlashCommand::Resume);
    let out = render(&app);
    println!("--- after /resume (stub) ---\n{out}\n--- end ---");
    assert!(
        out.contains("no session store wired"),
        "stub /resume should report no store wired:\n{out}"
    );
    assert!(
        !app.resume_picker.open,
        "picker must not open without a lister"
    );
}

#[test]
fn test_context_shows_visual_usage() {
    let mut app = working();
    app.run_command(SlashCommand::Context);
    // The grid block is ~25 rows; render taller than the default 28 so the
    // inline block is not clipped.
    let out = render_text(&app, 100, 45);
    println!("--- /context ---\n{out}\n--- end ---");
    // The /context grid renders inline as conversation content (not a popup):
    // bold "Context Usage" header, grid+legend side-by-side, drill-down
    // sections, and Suggestions. The legend header and bold title must be
    // present.
    assert!(out.contains("Context Usage"), "context header missing");
    assert!(
        out.contains("Estimated usage by category"),
        "legend header missing"
    );
}

#[test]
fn test_status_shows_runtime_state() {
    let mut app = working();
    app.run_command(SlashCommand::Status);
    let out = render(&app);
    println!("--- /status ---\n{out}\n--- end ---");
    // Status tab identity fields: Model + sandbox + Session ID. mode + breaker
    // moved to the Config tab (a focused config view); tokens + wall duration
    // moved to the Usage tab.
    assert!(out.contains("Model:"), "model field missing");
    assert!(out.contains("sandbox:"), "sandbox field missing");
    assert!(out.contains("Session ID:"), "session field missing");
    assert!(out.contains("Auth token:"), "auth token field missing");
    assert!(
        out.contains("Setting sources:"),
        "setting sources field missing"
    );
}

#[test]
fn test_model_opens_pane() {
    let mut app = working();
    app.run_command(SlashCommand::Model);
    assert_eq!(app.pane, crate::state::Pane::Model, "/model opens the pane");
    // Simulate the ModelInfo reply landing (stub app has no session to fetch).
    app.model_catalog = ModelCatalog {
        active_id: None,
        effort_level: None,
        catalog: vec![ModelCatalogEntry {
            id: "glm-5.2".into(),
            display_name: Some("Fable".into()),
            description: None,
            effort: None,
        }],
    };
    let out = render(&app);
    // Default sentinel row + the catalog row both render.
    assert!(out.contains("Default"), "Default row missing");
    assert!(out.contains("Fable"), "catalog row missing");
}

/// An empty catalog renders the empty-state guidance footer (no panic).
#[test]
fn test_model_empty_catalog_guide() {
    let mut app = working();
    app.run_command(SlashCommand::Model);
    let out = render(&app);
    assert!(out.contains("Default"));
    assert!(out.contains("no catalog configured"), "guide: {out}");
}

#[test]
fn test_sandbox_shows_breaker_state() {
    let mut app = working();
    app.run_command(SlashCommand::Sandbox);
    let out = render(&app);
    println!("--- /sandbox ---\n{out}\n--- end ---");
    // The aggregate resource fence: breaker state (+ trip reason + cool-down
    // when Open). No-runner stub path reports no breaker wired honestly
    // rather than a canned deny-by-default string.
    assert!(out.contains("sandbox:"), "sandbox field missing");
    assert!(out.contains("breaker:"), "breaker field missing");
}

#[test]
fn test_utility_panes_switch() {
    let mut app = working();
    app.run_command(SlashCommand::Graph);
    let g = render(&app);
    println!("--- /graph ---\n{g}\n--- end ---");
    assert_eq!(app.pane, Pane::Graph);
    assert!(g.contains("impact set"), "graph content missing");
    app.run_command(SlashCommand::Memory);
    let m = render(&app);
    println!("--- /memory ---\n{m}\n--- end ---");
    assert_eq!(app.pane, Pane::Memory);
    assert!(m.contains("build-gate"), "memory content missing");
    app.run_command(SlashCommand::Agents);
    let a = render(&app);
    println!("--- /agents ---\n{a}\n--- end ---");
    assert_eq!(app.pane, Pane::Agents);
    assert!(a.contains("explore"), "agents directory content missing");
}

#[test]
fn test_compact_honest_and_preserves() {
    let mut app = working();
    // Grow the transcript so a fake trim would have something to drop.
    for _ in 0..10 {
        app.system_line("a long line of transcript history");
    }
    let before = app.transcript.len();
    app.run_command(SlashCommand::Compact);
    let out = render(&app);
    println!("--- /compact ---\n{out}\n--- end ---");
    // Without a server, compact adds one diagnostic and preserves the transcript.
    assert!(
        out.contains("no server connected"),
        "honest no-server message missing: {out}"
    );
    assert_eq!(
        app.transcript.len(),
        before + 1,
        "compact must not drain prior lines (only its own system line is added)"
    );
}

#[test]
fn test_help_text_visible() {
    let mut app = working();
    app.run_command(SlashCommand::ReleaseNotes);
    let r = render(&app);
    println!("--- /release-notes ---\n{r}\n--- end ---");
    assert!(r.contains("what's new"), "release notes missing");
    app.run_command(SlashCommand::Help);
    let h = render(&app);
    println!("--- /help ---\n{h}\n--- end ---");
    assert!(h.contains("show help"), "help missing");
    // /tips is TUI-local (not in SlashCommand); submit via input
    app.input.set("/tips".to_string());
    app.submit_input();
    let t = render(&app);
    println!("--- /tips ---\n{t}\n--- end ---");
    assert!(t.contains("tips"), "tips missing");
}

#[test]
fn test_misc_commands_visible() {
    let mut app = working();
    app.run_command(SlashCommand::Worktree);
    let w = render(&app);
    println!("--- /worktree ---\n{w}\n--- end ---");
    assert!(w.contains("worktrees"), "worktree list missing");
    app.run_command(SlashCommand::Skills);
    let s = render(&app);
    println!("--- /skills ---\n{s}\n--- end ---");
    assert!(
        s.contains("skills discovered"),
        "skills pane content missing"
    );
    app.run_command(SlashCommand::Replay);
    let rp = render(&app);
    println!("--- /replay ---\n{rp}\n--- end ---");
    assert!(app.replaying, "replaying flag not set");
    assert!(rp.contains("replay"), "replay indicator missing");
}

#[test]
fn test_login_console_exit_commands() {
    let mut app = working();
    app.run_command(SlashCommand::Login);
    assert_eq!(app.screen, Screen::Login);
    app.screen = Screen::Working;
    app.run_command(SlashCommand::Console);
    assert_eq!(app.screen, Screen::Console);
    app.screen = Screen::Working;
    app.run_command(SlashCommand::Exit);
    assert!(app.quit, "/exit should quit");
}

/// /trajectory renders the audit log: one row per event with kind / ts /
/// id / prev_hash. The first event has no predecessor (prev:—); each later row
/// carries the short hash linking it into the chain. Pure-fn test on canned
/// wire entries — the chain order is covered in the session crate.
#[test]
fn test_trajectory_renders_chain() {
    use houyicoder_protocol::frontend::trajectory::TrajectoryEntry;
    let e1 = TrajectoryEntry {
        kind: "user".into(),
        ts: 100,
        event_id: "01HEVENT1".into(),
        prev_hash: None,
        duration_ms: None,
    };
    let e2 = TrajectoryEntry {
        kind: "assistant".into(),
        ts: 200,
        event_id: "01HEVENT2".into(),
        prev_hash: Some("abababab".into()),
        duration_ms: None,
    };
    let out = crate::command::render::render_trajectory_wire(&[e1, e2], &[]);
    assert!(out.starts_with("trajectory: 2 events"), "{out}");
    assert!(out.contains("user"), "{out}");
    assert!(out.contains("assistant"), "{out}");
    assert!(
        out.contains("prev:—"),
        "first event shows no predecessor: {out}"
    );
    assert!(
        out.contains("prev:abababab"),
        "second event shows its hash link: {out}"
    );
}
