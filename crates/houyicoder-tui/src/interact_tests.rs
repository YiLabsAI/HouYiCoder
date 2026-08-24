//! Interaction buffer-dump tests: each test drives an interaction (slash
//! command, approval, navigation, session management), renders the App to a
//! TestBackend, and asserts on the actual rendered text so behavior is
//! confirmed from real output rather than guessing. Run with --nocapture to
//! inspect the dumps. Data is canned; behavior is real.

#![cfg(test)]

use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::model::{ModelCatalog, ModelCatalogEntry};

use crate::composition;
use crate::state::{Divergence, Pane, Screen, Stage, Verdict};
use crate::test_support::render_text;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    // Seed a sample agent directory so the stub /agents pane renders real
    // directory content (production fetches this via the /agents query).
    app.agent_directory = Some("## Available agents\n\n- explore: fast read-only search".into());
    app
}

#[test]
fn test_paste_submit_expands() {
    let mut app = working();
    let big = "x".repeat(900);
    let token = app.pasted.ingest(&big);
    assert!(token.starts_with("[Pasted text #"));
    app.input.set(token);
    app.submit_input();
    // The echoed User line must be the expanded real text, not the token.
    let user = app.transcript.iter().find_map(|l| match l {
        crate::state::TranscriptLine::User(s) => Some(s.clone()),
        _ => None,
    });
    assert_eq!(user.as_deref(), Some(big.as_str()));
    // Store is NOT cleared after submit — ids increment across the session
    // (entries persist for the session lifetime).
    assert!(!app.pasted.is_empty(), "store retains entries after submit");
}

#[test]
fn test_two_pastes_submit_distinct() {
    let mut app = working();
    let a = "first ".to_string() + &"a".repeat(900);
    let b = "second ".to_string() + &"b".repeat(900);
    // first paste + submit
    app.input.set(app.pasted.ingest(&a));
    app.submit_input();
    let first = app
        .transcript
        .iter()
        .rev()
        .find_map(|l| match l {
            crate::state::TranscriptLine::User(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(first, a);
    // second paste + submit — must send b, not a
    app.input.set(app.pasted.ingest(&b));
    app.submit_input();
    let second = app
        .transcript
        .iter()
        .rev()
        .find_map(|l| match l {
            crate::state::TranscriptLine::User(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(
        second, b,
        "second send must be the second paste, not the first"
    );
}

/// Render at 100x28 so panes have room to show real content.
fn render(app: &crate::state::App) -> String {
    render_text(app, 100, 28)
}

#[test]
fn test_scrollback_keeps_top_row() {
    // Regression: a separate transcript top-rule row used to sit on the first
    // transcript line, so scrolling back to the top covered row-0 with a ─
    // line and stole one row of capacity. The rule is gone now; the live
    // signal is the terminal emulator's own tab-bar progress shimmer (OSC
    // 9;4). Scrolled to the top, row-0 must be the first visible content line.
    let mut app = working();
    for i in 0..40 {
        app.push_transcript_line(crate::state::TranscriptLine::System(format!("row-{i}")));
    }
    app.transcript_scroll.follow_tail = false;
    app.transcript_scroll.offset = 0;
    let out = render_text(&app, 80, 12);
    println!("--- scrollback top (80x12) ---\n{out}\n--- end ---");
    let first = out.lines().next().unwrap_or("");
    assert!(
        first.contains("row-0"),
        "first transcript line must be row-0 content, not a rule: [{first}]\n{out}"
    );
}

#[test]
fn test_spinner_verb_reflects_phase() {
    // The spinner verb must track the live phase, not a mount-fixed random
    // word: streaming reasoning ⇒ Thinking, otherwise Working. (A per-session
    // hash verb mislabeled every turn as "Optimizing" with no link to reality.)
    let mut app = working();
    app.agent_busy = true;
    app.run_started = Some(std::time::Instant::now());
    // Reasoning streaming ⇒ Thinking.
    app.live_reasoning_text = "pondering the task".to_string();
    app.live_block = crate::state::enums::LiveBlock::Thinking;
    let out = render_text(&app, 80, 12);
    assert!(
        out.contains("Thinking"),
        "reasoning phase should show Thinking:\n{out}"
    );
    // No reasoning streaming ⇒ Working.
    app.live_reasoning_text.clear();
    app.live_block = crate::state::enums::LiveBlock::None;
    let out = render_text(&app, 80, 12);
    assert!(
        out.contains("Working"),
        "non-reasoning phase should show Working:\n{out}"
    );
    assert!(
        !out.contains("Optimizing"),
        "stale random verb must be gone:\n{out}"
    );
}

/// Regression: once reasoning streamed, the verb must NOT stay Thinking for
/// the rest of the turn while assistant text is the active stream. The live
/// block flips to Responding on an assistant-text Delta, so the verb reads
/// Working even though live_reasoning_text still holds the streamed reasoning
/// (it is kept for the post-turn ThoughtFor summary). The old test used a
/// sticky "reasoning text non-empty" signal that locked Thinking all turn.
#[test]
fn test_verb_works_text_streams() {
    let mut app = working();
    app.agent_busy = true;
    app.run_started = Some(std::time::Instant::now());
    // Reasoning streamed first, then assistant text takes over.
    app.live_reasoning_text = "pondered".to_string();
    app.live_assistant_text = "Here is the answer".to_string();
    app.live_block = crate::state::enums::LiveBlock::Responding;
    let out = render_text(&app, 80, 12);
    assert!(
        out.contains("Working"),
        "text streaming should show Working, not Thinking:\n{out}"
    );
    assert!(
        !out.contains("Thinking"),
        "stale Thinking must not persist once text streams:\n{out}"
    );
}

#[test]
fn test_busy_border_stays_plain() {
    // The live "still processing" signal is the terminal emulator's own
    // tab-bar progress shimmer (OSC 9;4, see app.rs set_terminal_progress),
    // NOT an in-app border animation. Busy or idle, no border row may carry
    // gradient cyan sweep cells — a regression guard against the removed
    // in-app shimmer coming back.
    use ratatui::style::Color;
    let is_shimmer = |c: Color| matches!(c, Color::Rgb(0, g, b) if g == b && g > 0);
    for busy in [true, false] {
        let mut app = working();
        app.agent_busy = busy;
        app.run_started = busy.then(std::time::Instant::now);
        let buf = crate::test_support::render_buffer(&app, 80, 16);
        for y in 0..16 {
            for x in 0..80 {
                let cell = buf.cell((x, y)).expect("cell");
                if is_shimmer(cell.fg) && cell.symbol() == "─" {
                    panic!("border must not carry an in-app shimmer at ({x},{y}), busy={busy}");
                }
            }
        }
    }
}

#[test]
fn test_design_approve_advances() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    let out = render(&app);
    println!("--- /spec (design) ---\n{out}\n--- end ---");
    assert!(out.contains("spec"), "spec pane title missing");
    assert!(out.contains("acceptance:"), "acceptance missing");
    assert_eq!(app.stage, Stage::Design);
    // one design approval -> implement (spec + plan merged into design)
    app.approve_in_pane();
    let out = render(&app);
    println!("--- after design approve ---\n{out}\n--- end ---");
    assert_eq!(app.stage, Stage::Implementing);
    assert_eq!(app.pane, Pane::Diff);
    assert!(out.contains("diff approval"), "diff pane not shown");
    assert!(out.contains("change 1/3"), "change counter missing");
    assert!(
        app.spec_artifact.approved,
        "spec artifact should be approved"
    );
    assert!(
        app.plan_artifact.approved,
        "plan artifact should be approved"
    );
}

#[test]
fn test_implement_approve_advances() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // design -> implement
    // approve all 3 changes via the per-pane action. Auto-advance moves focus
    // to the next pending change after each approve, so a repeated approve
    // walks every change and trips the all-approved transition to verify.
    for _ in 0..3 {
        app.approve_in_pane();
    }
    let out = render(&app);
    println!("--- after all changes approved ---\n{out}\n--- end ---");
    assert_eq!(
        app.stage,
        Stage::Verify,
        "should auto-advance to verify (agent review phase)"
    );
    assert_eq!(app.pane, Pane::Review);
    // requirement statuses moved unimpl -> partial (changes approved)
    assert!(
        app.spec_clauses
            .iter()
            .all(|c| c.status == Divergence::Partial)
    );
    assert!(out.contains("review findings"), "review pane not shown");
}

#[test]
fn test_review_signoff_advances() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // design -> implement
    for _ in 0..3 {
        app.approve_in_pane();
    }
    // approve all 3 findings (agent review phase)
    for _ in 0..3 {
        app.approve_in_pane();
        app.navigate_pane(true);
    }
    let out = render(&app);
    println!("--- after all findings approved ---\n{out}\n--- end ---");
    assert_eq!(
        app.stage,
        Stage::Verify,
        "stage stays verify across the two phases"
    );
    assert_eq!(app.pane, Pane::Verify, "should move to machine-check phase");
    assert!(out.contains("verify result"), "verify pane not shown");
}

#[test]
fn test_spec_chain_completes() {
    let mut app = working();
    app.run_command(SlashCommand::Spec);
    app.approve_in_pane(); // design -> implement
    for _ in 0..3 {
        app.approve_in_pane();
    }
    for _ in 0..3 {
        app.approve_in_pane();
        app.navigate_pane(true);
    }
    app.approve_in_pane(); // complete machine check -> done
    let out = render(&app);
    println!("--- after verify complete ---\n{out}\n--- end ---");
    assert_eq!(app.stage, Stage::Done);
    assert!(out.contains("DONE"), "completion indicator missing");
    assert!(
        app.spec_clauses
            .iter()
            .all(|c| c.status == Divergence::Satisfied),
        "clauses should be satisfied after verify"
    );
}

#[test]
fn test_reject_hunk_state() {
    let mut app = working();
    app.run_command(SlashCommand::Implement);
    app.reject_in_pane();
    let out = render(&app);
    println!("--- after hunk reject ---\n{out}\n--- end ---");
    assert_eq!(app.diff.current().unwrap().approved, Verdict::Rejected);
    assert!(out.contains("rejected"), "rejected state not visible");
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
    // In stub mode (no session_lister wired), /resume reports that no session
    // store is wired instead of opening the picker. The real picker-over-
    // sessions path is in resume_picker_tests.rs (a wired bundle lists +
    // switches sessions).
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

/// The /memory pane renders the auto-memory + auto-dream toggle rows and
/// reflects their on/off state. Pins the pane-template render (the rows live
/// in the lower band via the shared command-pane shape, not the old
/// capability-column list) so a later refactor cannot silently drop them.
#[test]
fn test_memory_pane_renders_rows() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let on = render(&app);
    assert_eq!(app.pane, Pane::Memory);
    assert!(on.contains("Auto-memory: on"), "auto-memory on row missing");
    assert!(on.contains("Auto-dream: on"), "auto-dream on row missing");
    assert!(
        on.contains("/memory toggle auto|dream"),
        "toggle hint missing"
    );
    // Flip both off in the view state; the rows re-render off without a
    // round-trip (the wire result applies the snapshot before re-render).
    app.memory_toggles.auto_memory = false;
    app.memory_toggles.auto_dream = false;
    let off = render(&app);
    assert!(
        off.contains("Auto-memory: off"),
        "auto-memory off row missing"
    );
    assert!(
        off.contains("Auto-dream: off"),
        "auto-dream off row missing"
    );
}

/// /memory toggle <which> in stub mode (no carrier) reports no carrier rather
/// than crashing. Pins the toggle-subcommand parse + the None-carrier branch so
/// a later refactor cannot silently drop the command.
#[test]
fn test_memory_toggle_no_carrier() {
    let mut app = working();
    let handled = app.run_tui_local_command("memory toggle auto");
    assert!(handled, "toggle subcommand handled");
    let out = render(&app);
    assert!(
        out.contains("no carrier"),
        "stub mode reports no carrier:\n{out}"
    );
}

/// /memory toggle with a bad argument names the usage so the user learns the
/// two valid switches, not a silent no-op.
#[test]
fn test_memory_toggle_bad_arg() {
    let mut app = working();
    let handled = app.run_tui_local_command("memory toggle bogus");
    assert!(handled, "bad toggle still handled (usage shown)");
    let out = render(&app);
    assert!(
        out.contains("/memory toggle auto|dream"),
        "bad arg names the usage:\n{out}"
    );
}

/// An empty memory store renders the empty hint + a zero count, not a blank
/// pane. Pins the empty-list branch in the pane body.
#[test]
fn test_memory_pane_empty_state() {
    let mut app = working();
    app.memory_entries.clear();
    app.run_command(SlashCommand::Memory);
    let out = render(&app);
    assert!(out.contains("0 stored"), "zero count missing:\n{out}");
    assert!(
        out.contains("no memories yet"),
        "empty hint missing:\n{out}"
    );
}

/// Each row carries a scope dot source tag (e.g. project dot project) so the
/// two orthogonal dimensions — storage root + provenance — are both visible.
/// Pins the tag render the scope filter + per-row classification ride.
#[test]
fn test_memory_pane_renders_tag() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    // The stub seeds build-gate as scope=project, source=project.
    let out = render_text(&app, 100, 28);
    assert!(
        out.contains("[project·project] build-gate"),
        "scope dot source tag missing:\n{out}"
    );
}

/// Shift+Tab cycles the scope tab (All → User → Project → Auto) and the list
/// narrows to the matching physical root. Pins the "see this project's
/// memories" filter — the dimension orthogonal to the provenance source.
#[test]
fn test_scope_tab_narrows_list() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    // All: the stub seeds three (project / user / auto scopes).
    let all = render(&app);
    assert!(all.contains("3 stored"), "All shows all three:\n{all}");
    assert!(all.contains("[All]"), "All tab active");
    // User: only comment-style lives in the user root.
    app.cycle_memory_scope();
    let user = render(&app);
    assert!(user.contains("1 stored"), "User narrows to one:\n{user}");
    assert!(user.contains("comment-style"), "user-scoped entry shows");
    assert!(
        !user.contains("build-gate"),
        "project-scoped hidden under User"
    );
    assert!(user.contains("[User]"), "User tab active");
    // Project: only build-gate.
    app.cycle_memory_scope();
    let project = render(&app);
    assert!(project.contains("build-gate"), "project-scoped shows");
    assert!(
        !project.contains("comment-style"),
        "user-scoped hidden under Project"
    );
    assert!(project.contains("[Project]"), "Project tab active");
    // Auto: only spec-driven.
    app.cycle_memory_scope();
    let auto = render(&app);
    assert!(auto.contains("spec-driven"), "auto-scoped shows");
    assert!(auto.contains("[Auto]"), "Auto tab active");
    // One more cycle wraps back to All (covers the Auto to All arm).
    app.cycle_memory_scope();
    let back = render(&app);
    assert!(back.contains("[All]"), "cycle wraps to All");
    assert!(back.contains("3 stored"), "All shows all three again");
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
    // Honest: with no server wired, /compact reports it cannot reach the
    // server instead of pretending. The transcript is NOT drained — the
    // previous fake stub removed the old render lines, which looked real and
    // was not. The dispatch adds exactly one system line and keeps every
    // prior line intact.
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
    app.run_command(SlashCommand::PasteImage);
    let p = render(&app);
    println!("--- /paste-image ---\n{p}\n--- end ---");
    assert!(p.contains("TUI"), "paste-image limitation missing");
    app.run_command(SlashCommand::Voice);
    let v = render(&app);
    println!("--- /voice ---\n{v}\n--- end ---");
    assert!(v.contains("whisper"), "voice placeholder missing");
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

/// /trajectory projects the audit log: one row per event with kind / ts /
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
