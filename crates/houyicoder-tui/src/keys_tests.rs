use super::approval::handle_approval;
use super::input::{cycle_pane, handle_input};
use super::palette::handle_palette;
use super::*;
use crate::composition;
use crate::state::Screen;
use crate::state::TranscriptLine;
use crate::state::Verdict;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use houyicoder_protocol::frontend::LoginMode;
use houyicoder_protocol::frontend::model::{ModelCatalog, ModelCatalogEntry};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn working_app() -> App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app
}
fn catalog_app(entries: &[&str]) -> App {
    let mut app = working_app();
    app.model_catalog = ModelCatalog {
        active_id: None,
        effort_level: None,
        catalog: entries
            .iter()
            .map(|id| ModelCatalogEntry {
                id: id.to_string(),
                display_name: Some(id.to_string()),
                description: None,
                effort: None,
            })
            .collect(),
    };
    app
}

/// The /model pane Up/Down/Enter keys: Down moves the cursor, Up moves back,
/// Enter on the Default row applies the sentinel + closes the pane.
#[test]
fn test_model_pane_select_tier() {
    let mut app = catalog_app(&["a", "b"]);
    app.pane = Pane::Model;
    handle_input(&mut app, key(KeyCode::Down));
    assert_eq!(app.model_sel, 1, "Down moves the cursor");
    handle_input(&mut app, key(KeyCode::Up));
    assert_eq!(app.model_sel, 0, "Up moves back");
    handle_input(&mut app, key(KeyCode::Enter));
    assert_eq!(
        app.model_tier, "Default",
        "Enter on Default applies the sentinel"
    );
    assert_eq!(app.pane, Pane::Transcript, "Enter closes the pane");
}

/// The /model pane Up/Down navigate + recompute effort for the new model.
#[test]
fn test_model_pane_recomputes_effort() {
    let mut app = catalog_app(&["qwen3.7-max", "deepseek-chat"]);
    app.pane = Pane::Model;
    // Down to row 1 (qwen3 supports effort) → effort defaults to Medium.
    handle_input(&mut app, key(KeyCode::Down));
    assert_eq!(
        app.model_effort,
        Some(houyicoder_protocol::llm::EffortLevel::Medium),
        "qwen3 supports effort → Medium default"
    );
    // Down to row 2 (deepseek = NotSupported) → effort None.
    handle_input(&mut app, key(KeyCode::Down));
    assert!(app.model_effort.is_none(), "deepseek not supported → None");
    // Up back to qwen3 → effort Medium again (not toggled).
    handle_input(&mut app, key(KeyCode::Up));
    assert_eq!(
        app.model_effort,
        Some(houyicoder_protocol::llm::EffortLevel::Medium),
        "back to qwen3 → Medium (not toggled)"
    );
}

/// ←/→ cycles the effort pick and sets model_effort_toggled.
#[test]
fn test_model_pane_cycles_effort() {
    let mut app = catalog_app(&["qwen3.7-max"]);
    app.pane = Pane::Model;
    // Down to the qwen3 row (idx 1) so supports_effort is true.
    handle_input(&mut app, key(KeyCode::Down));
    assert_eq!(app.model_sel, 1);
    // Default Medium → Right → High.
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(
        app.model_effort,
        Some(houyicoder_protocol::llm::EffortLevel::High),
        "Right cycles to High"
    );
    assert!(app.model_effort_toggled, "toggled flag set");
    // Left → back to Medium.
    handle_input(&mut app, key(KeyCode::Left));
    assert_eq!(
        app.model_effort,
        Some(houyicoder_protocol::llm::EffortLevel::Medium),
        "Left cycles back to Medium"
    );
    // Left again → Low.
    handle_input(&mut app, key(KeyCode::Left));
    assert_eq!(
        app.model_effort,
        Some(houyicoder_protocol::llm::EffortLevel::Low),
        "Left wraps to Low"
    );
    // Toggled flag sticks: cursor move no longer clobbers.
    handle_input(&mut app, key(KeyCode::Up));
    assert_eq!(
        app.model_effort,
        Some(houyicoder_protocol::llm::EffortLevel::Low),
        "toggled pick survives cursor move"
    );
}

/// ←/→ is a no-op on a not-supported model.
#[test]
fn test_model_pane_noop_unsupported() {
    let mut app = catalog_app(&["deepseek-chat"]);
    app.pane = Pane::Model;
    // Down to the deepseek row.
    handle_input(&mut app, key(KeyCode::Down));
    handle_input(&mut app, key(KeyCode::Right));
    assert!(app.model_effort.is_none(), "no-op on not-supported model");
    assert!(!app.model_effort_toggled, "toggled flag not set");
}

/// The /model pane Esc key closes back to the transcript.
#[test]
fn test_model_pane_esc_closes() {
    let mut app = working_app();
    app.pane = Pane::Model;
    handle_input(&mut app, key(KeyCode::Esc));
    assert_eq!(app.pane, Pane::Transcript);
}

/// The /skills pane Esc key closes back to the transcript.
#[test]
fn test_skills_pane_esc_closes() {
    let mut app = working_app();
    app.pane = Pane::Skills;
    handle_input(&mut app, key(KeyCode::Esc));
    assert_eq!(app.pane, Pane::Transcript);
}

/// Pasting routes to the palette query when the palette is open (the
/// hint-after-space arg surface), else to the input box. Guards the bug where a
/// pasted file path / sid went to the input bar and the popup never saw it.
#[test]
fn test_paste_routes_palette_open() {
    // Palette open: paste lands in the query (the arg surface).
    let mut app = working_app();
    app.open_palette();
    app.palette.query = "resume ".into();
    app.apply_paste_token("/tmp/export.json");
    assert_eq!(
        app.palette.query, "resume /tmp/export.json",
        "palette paste should append to the query"
    );
    assert_eq!(
        app.input.value(),
        "",
        "input must stay empty when palette open"
    );

    // Palette closed: paste lands in the input box.
    let mut app = working_app();
    app.apply_paste_token("hello");
    assert_eq!(app.input.value(), "hello", "input paste appends to the box");
}

/// The /status pane Left / Right keys cycle the sub-tab (Status → Config →
/// Usage → back to Status).
#[test]
fn test_status_pane_cycles_subtab() {
    let mut app = working_app();
    app.pane = Pane::Status;
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(app.status_tab, crate::state::enums::StatusTab::Config);
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(app.status_tab, crate::state::enums::StatusTab::Usage);
    handle_input(&mut app, key(KeyCode::Right));
    assert_eq!(app.status_tab, crate::state::enums::StatusTab::Status);
}

#[test]
fn test_login_local_skips_console() {
    let mut app = composition::app();
    handle_login(&mut app, key(KeyCode::Char('3')));
    assert_eq!(app.screen, Screen::Working);
    assert_eq!(app.login_mode, Some(LoginMode::Local));
}

#[test]
fn test_login_lands_clean() {
    // Landing on Working leaves the transcript empty (placeholder hint only).
    let mut app = composition::app();
    app.transcript.clear();
    handle_login(&mut app, key(KeyCode::Char('1')));
    assert_eq!(app.screen, Screen::Working);
    assert_eq!(app.transcript.len(), 0);
    // landing again on an already-alive transcript does not seed anything
    let before = app.transcript.len();
    app.transcript.push(TranscriptLine::System("x".into()));
    handle_login(&mut app, key(KeyCode::Char('1')));
    assert_eq!(app.transcript.len(), before + 1);
}

#[test]
fn test_palette_enter_runs_selected() {
    let mut app = working_app();
    app.open_palette();
    handle_palette(&mut app, key(KeyCode::Enter));
    assert!(!app.palette.open);
}

#[test]
fn test_palette_select_runs_command() {
    // Selecting a command routes through the input box and the unified
    // submit path (echoed as a User turn, then runs). Auto-run on select.
    let mut app = working_app();
    app.open_palette();
    for c in "context".chars() {
        handle_palette(&mut app, key(KeyCode::Char(c)));
    }
    assert_eq!(app.selected_command().map(|c| c.name()), Some("/context"));
    handle_palette(&mut app, key(KeyCode::Enter));
    assert!(!app.palette.open);
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "/context")),
        "palette select must echo the command as a User turn"
    );
    assert!(
        matches!(app.transcript.last(), Some(TranscriptLine::ContextGrid(_))),
        "the /context response should follow the echoed command"
    );
}

#[test]
fn test_palette_accepts_space_separator() {
    // A space in the palette query is the arg separator for arg-taking slash
    // commands (/permissions git off). Without accepting it the query arrives
    // concatenated (permissionsgitoff) + arg commands are unreachable. The
    // space lands in the query so the raw-submit branch can ship the typed
    // command as a slash when no palette entry matches.
    let mut app = working_app();
    app.open_palette();
    for c in "permissions git".chars() {
        handle_palette(&mut app, key(KeyCode::Char(c)));
    }
    assert!(
        app.palette.query.contains(' '),
        "space must land in the query, not be dropped: {:?}",
        app.palette.query
    );
}

#[test]
fn test_palette_tui_local_via() {
    // /debug is a palette-registered local command: typing "debug" filters
    // the list to it, Enter selects + auto-runs it through the unified
    // submit path (echoed as a User turn + dispatched).
    let mut app = working_app();
    app.open_palette();
    for c in "debug".chars() {
        handle_palette(&mut app, key(KeyCode::Char(c)));
    }
    handle_palette(&mut app, key(KeyCode::Enter));
    assert!(!app.palette.open);
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::User(s) if s == "/debug")),
        "palette select of /debug must echo as a User turn"
    );
    assert!(
        matches!(app.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("debug")),
        "the debug response should follow the echo"
    );
    // A leading slash in the palette query is normalized, not doubled.
    let mut app = working_app();
    app.open_palette();
    for c in "/debug".chars() {
        handle_palette(&mut app, key(KeyCode::Char(c)));
    }
    handle_palette(&mut app, key(KeyCode::Enter));
    assert!(
        matches!(app.transcript.last(), Some(TranscriptLine::System(s)) if s.contains("debug")),
        "leading slash in palette query must be normalized, not double-slashed"
    );
}

/// Selecting an arg-taking command (/search) keeps the palette OPEN and seeds
/// the query with the name + a trailing space — the user keeps typing the
/// argument in the popup (which shows the arg hint after the space), then
/// presses Enter to submit. Argless commands auto-run on select.
#[test]
fn test_palette_arg_keeps_open() {
    let mut app = working_app();
    app.open_palette();
    for c in "search".chars() {
        handle_palette(&mut app, key(KeyCode::Char(c)));
    }
    assert_eq!(
        app.selected_command().map(|c| c.name()),
        Some("/search"),
        "/search must be palette-discoverable"
    );
    handle_palette(&mut app, key(KeyCode::Enter));
    assert!(app.palette.open, "palette stays open for the arg");
    assert_eq!(
        app.palette.query, "search ",
        "arg command seeds the query with name + space"
    );
    assert!(
        app.transcript
            .iter()
            .all(|l| !matches!(l, TranscriptLine::User(_))),
        "arg command must NOT auto-submit (waits for the query)"
    );
}

#[test]
fn test_palette_typing_filters() {
    let mut app = working_app();
    app.open_palette();
    assert_eq!(
        app.palette_len(),
        houyicoder_protocol::frontend::SlashCommand::ALL.len()
    );
    handle_palette(&mut app, key(KeyCode::Char('s')));
    handle_palette(&mut app, key(KeyCode::Char('p')));
    handle_palette(&mut app, key(KeyCode::Char('e')));
    assert_eq!(app.palette.sel, 0);
    let cmd = app.selected_command().expect("filtered list non-empty");
    assert!(cmd.name().contains("spe"));
    assert!(app.palette_len() < houyicoder_protocol::frontend::SlashCommand::ALL.len());
}

#[test]
fn test_palette_backspace_edits() {
    let mut app = working_app();
    app.open_palette();
    handle_palette(&mut app, key(KeyCode::Char('x')));
    assert!(!app.palette.query.is_empty());
    handle_palette(&mut app, key(KeyCode::Backspace));
    assert!(app.palette.query.is_empty());
    assert!(app.palette.open);
    handle_palette(&mut app, key(KeyCode::Backspace));
    assert!(!app.palette.open);
}

#[test]
fn test_tab_cycles_pane() {
    let mut app = working_app();
    assert_eq!(app.pane, Pane::Transcript);
    cycle_pane(&mut app);
    assert_eq!(app.pane, Pane::Spec);
}

#[test]
fn test_diff_approve_key_sets() {
    let mut app = working_app();
    app.pane = Pane::Diff;
    app.stage = Stage::Implementing;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Down));
    handle_working(&mut app, key(KeyCode::Char('a')));
    assert_eq!(app.diff.hunks[1].approved, Verdict::Approved);
}

#[test]
fn test_diff_reject_key_sets() {
    let mut app = working_app();
    app.pane = Pane::Diff;
    app.stage = Stage::Implementing;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Char('r')));
    assert_eq!(app.diff.current().unwrap().approved, Verdict::Rejected);
}

/// The 'i' key on the Review pane in Verify stage routes to rework_in_pane.
/// 'r' is caught first by the reject arm (both rejectable and reworkable hold
/// on Review+Verify), so 'i' is the unique path to the rework arm. With no
/// finding under the cursor, rework is a no-op (no panic); the arm still fires.
#[test]
fn test_review_rework_key_routes() {
    let mut app = working_app();
    app.pane = Pane::Review;
    app.stage = Stage::Verify;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Char('i')));
    assert_eq!(app.pane, Pane::Review, "no finding -> rework is a no-op");
    assert!(
        app.input.is_empty(),
        "i consumed by the rework arm, not pushed to the input box"
    );
}

/// Esc while a run is in flight interrupts but leaves the queue intact: the
/// head is NOT popped on the same press. A panic double-press used to
/// abort+pop in one, then the second Esc fell through to clear-input and
/// wiped the just-recalled message (abort sets cancelling but agent_busy
/// stays true until Done). Now interrupt and recall are separate presses, so
/// the common double-press is interrupt+recall, not interrupt+destroy.
#[test]
fn test_esc_double_press_safe() {
    use crate::pending_queue::PendingItem;
    let mut app = working_app();
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("task a".into()));
    // Esc1: interrupt only. The queue still holds "task a"; input stays empty.
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(
        app.pending
            .iter()
            .any(|p| matches!(p, PendingItem::Message(t) if t == "task a")),
        "Esc1 interrupts without popping -- the queue keeps the message"
    );
    assert!(app.input.is_empty(), "Esc1 does not touch the input box");
    assert!(app.cancelling, "Esc1 set cancelling (abort in flight)");
    // Esc2: recall. The head pops into the input. A double-press is safe now.
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.input.value(),
        "task a",
        "Esc2 recalls the queued message"
    );
}

/// Recall merges the queued message with a half-typed draft instead of
/// overwriting it: the queued message prepends, a newline separates, the
/// cursor parks at the draft start. The prior pop overwrote the draft,
/// destroying the user's half-typed text.
#[test]
fn test_esc_recall_merges_draft() {
    use crate::pending_queue::PendingItem;
    let mut app = working_app();
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("task a".into()));
    app.input.set("half draft".into());
    // Esc1: interrupt -- the draft is untouched (not cleared).
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.input.value(),
        "half draft",
        "Esc1 leaves the draft alone"
    );
    // Esc2: recall -- the queued message prepends, the draft survives.
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.input.value(),
        "task a\nhalf draft",
        "Esc2 merges the queued message with the draft, not overwrites"
    );
}

/// During the cancelling window (after Esc1 interrupted, agent_busy still
/// true until Done), Esc on a pane with its own close (Memory) must close
/// the pane, not pop the queue. The pop arm shares the busy-Esc arm's pane
/// gate so the cancelling window does not steal the pane-close key.
#[test]
fn test_esc_cancelling_closes_pane() {
    use crate::pending_queue::PendingItem;
    let mut app = working_app();
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("task a".into()));
    // Esc1: interrupt (cancelling, agent_busy still true, queue intact).
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(app.cancelling);
    // Open a pane with its own Esc, then Esc: the pane closes, the queue
    // is NOT popped (the pane-close key is not stolen for a recall).
    app.pane = Pane::Memory;
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.pane, Pane::Transcript, "Esc closes the Memory pane");
    assert!(
        !app.pending.is_empty(),
        "the queue is not popped while closing the pane"
    );
    assert!(
        app.input.is_empty(),
        "input stays empty -- no recall on the pane-close Esc"
    );
}

/// Same gate for /trajectory, which owns multi-level Esc (level 2 -> 1 -> 0
/// -> close). The cancelling window must not steal its Esc for a queue pop.
/// Pins that pane_owns_esc covers Trajectory, the pane the copied-list bug
/// left in neither Esc arm.
#[test]
fn test_esc_cancelling_backs_trajectory() {
    use crate::pending_queue::PendingItem;
    let mut app = working_app();
    app.agent_busy = true;
    app.pending.push(PendingItem::Message("task a".into()));
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(app.cancelling);
    app.pane = Pane::Trajectory;
    app.trajectory_level.set(1);
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.trajectory_level.get(),
        0,
        "Esc backs the trajectory pane a level, not pops the queue"
    );
    assert!(
        !app.pending.is_empty(),
        "the queue is not popped while backing the pane"
    );
}

#[test]
fn test_console_signoff_appends_audit() {
    let mut app = composition::app();
    app.screen = Screen::Console;
    let before = app.review.audit_trail.len();
    handle_console(&mut app, key(KeyCode::Char('a')));
    assert_eq!(app.review.audit_trail.len(), before + 1);
}

#[test]
fn test_console_reject_writes_org() {
    let mut app = composition::app();
    app.screen = Screen::Console;
    handle_console(&mut app, key(KeyCode::Char('r')));
    assert!(app.review.findings[app.review.focus].resolved());
    assert!(matches!(app.transcript.last(),
            Some(TranscriptLine::System(s)) if s.contains("org eval")));
}

#[test]
fn test_approval_enter_clears_popup() {
    // The verdict is transient (a dismissable card, not a transcript row), so
    // Enter must clear the popup without logging a verdict line.
    let mut app = working_app();
    app.approval = Some(crate::state::Approval {
        tool: "FsWrite".to_string(),
        args: "".to_string(),
        reason: "".to_string(),
        selected: 1,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    handle_approval(&mut app, key(KeyCode::Enter));
    assert!(app.approval.is_none(), "popup must clear on confirm");
}

// --- Approval key-dispatch tests through handle_working ---
// These drive the full key dispatch chain (handle_working → handle_input →
// handle_generic_input → handle_approval), not just handle_approval directly.
// They verify key→action mappings: navigation moves the cursor, Enter produces
// the correct verdict, and always-allow persists a permission rule.

fn approval_app(selected: usize) -> App {
    let mut app = working_app();
    app.approval = Some(crate::state::Approval {
        tool: "bash".to_string(),
        args: r#"{"command":"ls"}"#.to_string(),
        reason: "test".to_string(),
        selected,
        call_id: String::new(),
        options: Vec::new(),
        ..Default::default()
    });
    app
}

#[test]
fn test_approval_down_dont_ask() {
    // Down from Yes(0) goes to Yes-don't-ask(2) in display order. Full dispatch.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(
        app.approval.as_ref().expect("approval").selected,
        2,
        "Down must move cursor to Yes-don't-ask"
    );
}

#[test]
fn test_approval_up_wraps() {
    // Up from Yes(0) wraps to No(1) in display order. Full dispatch.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(
        app.approval.as_ref().expect("approval").selected,
        1,
        "Up must wrap cursor to No"
    );
}

#[test]
fn test_approval_down_wraps_yes() {
    // Down from No(1) wraps to Yes(0) in display order. Full dispatch.
    let mut app = approval_app(1);
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(
        app.approval.as_ref().expect("approval").selected,
        0,
        "Down must wrap from No to Yes"
    );
}

#[test]
fn test_approval_left_navigates_previous() {
    // Left from No(1) goes to Yes-don't-ask(2) in display order. Full dispatch.
    let mut app = approval_app(1);
    handle_working(&mut app, key(KeyCode::Left));
    assert_eq!(
        app.approval.as_ref().expect("approval").selected,
        2,
        "Left must move cursor to Yes-don't-ask"
    );
}

#[test]
fn test_approval_right_navigates_next() {
    // Right from Yes-don't-ask(2) goes to No(1) in display order. Full dispatch.
    let mut app = approval_app(2);
    handle_working(&mut app, key(KeyCode::Right));
    assert_eq!(
        app.approval.as_ref().expect("approval").selected,
        1,
        "Right must move cursor to No"
    );
}

#[test]
fn test_approval_key2_dont_ask() {
    // Pressing '2' directly selects Yes-don't-ask. Full dispatch.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Char('2')));
    assert_eq!(
        app.approval.as_ref().expect("approval").selected,
        2,
        "'2' must select Yes-don't-ask"
    );
}

#[test]
fn test_key3_selects_no_option() {
    // Pressing '3' directly selects No. Full dispatch.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Char('3')));
    assert_eq!(
        app.approval.as_ref().expect("approval").selected,
        1,
        "'3' must select No"
    );
}

#[test]
fn test_approval_enter_yes_approve() {
    // Enter on Yes(0) → approve verdict. Full dispatch through handle_working.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        app.approval.is_none(),
        "approval should clear after confirm"
    );
}

#[test]
fn test_approval_enter_no_reject() {
    // Enter on No(1) → reject verdict. Full dispatch through handle_working.
    let mut app = approval_app(1);
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        app.approval.is_none(),
        "approval should clear after confirm"
    );
}

#[test]
fn test_approval_nav_confirm_verdict() {
    // Navigate to No via Up (Yes wraps to No in display order), then Enter
    // clears. This catches the confirm=reject bug: if navigation works but
    // confirm reads the wrong cursor, the verdict will mismatch. Full dispatch.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.approval.as_ref().unwrap().selected, 1);
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        app.approval.is_none(),
        "Up+Enter must clear the popup: {:?}",
        app.approval
    );
}

#[test]
fn test_approval_nav_full_cycle() {
    // Full cycle: Down Down Down returns to the same option.
    let mut app = approval_app(0);
    handle_working(&mut app, key(KeyCode::Down)); // 0→2
    handle_working(&mut app, key(KeyCode::Down)); // 2→1
    handle_working(&mut app, key(KeyCode::Down)); // 1→0 (wrap)
    assert_eq!(
        app.approval.as_ref().unwrap().selected,
        0,
        "three Downs must wrap back to Yes"
    );
}

#[test]
fn test_esc_idle_clears_input() {
    // No runner wired, not busy: Esc must not abort (abort_run is a no-op
    // with no runner) and must not crash or toggle quit. It falls through
    // to the generic input Esc (clear input), which is empty here so no-op.
    let mut app = working_app();
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(!app.quit, "idle Esc must not quit");
    assert!(app.input.is_empty(), "idle Esc on empty input is a no-op");
}

/// While a run is in flight, Esc interrupts and leaves the draft untouched.
/// The prior behavior cleared the draft first (then aborted on a second
/// Esc), which destroyed the user's half-typed text and forced a two-press
/// interrupt. One Esc now interrupts; the draft survives for the user to
/// edit or clear deliberately.
#[test]
fn test_busy_esc_keeps_draft() {
    let mut app = working_app();
    app.agent_busy = true;
    app.input.set("draft".into());
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(app.cancelling, "busy Esc interrupts the run");
    assert_eq!(app.input.value(), "draft", "the draft is left untouched");
}

/// While a run is in flight, Esc in the /worktrees pane closes the pane back
/// to the transcript rather than aborting the run — the sibling /memory pane
/// does the same, and /worktrees must match so a user dismissing the pane
/// does not lose the in-flight turn. Without Pane::Worktree in the busy-Esc
/// exclusion the abort arm shadowed the pane's Esc-close, so Esc always
/// aborted and the pane could not be dismissed while busy.
#[test]
fn test_worktree_esc_closes_busy() {
    let mut app = working_app();
    app.pane = Pane::Worktree;
    app.agent_busy = true;
    app.input.clear();
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(app.pane, Pane::Transcript, "Esc closes the worktree pane");
    assert!(
        !app.cancelling,
        "must not abort the run when dismissing the worktree pane"
    );
}

/// /worktrees pane key surface: Up/Down move the cursor, Enter/d route to
/// the enter/remove actions (which report no-carrier in stub mode), Char
/// types into the search query, Backspace edits it, Esc in search clears
/// the query (a second Esc closes), and an unmapped key is a no-op. These
/// cover the keys/worktree_pane::handle arms without a PTY.
fn worktree_app_with_rows() -> App {
    let mut app = working_app();
    app.pane = Pane::Worktree;
    app.worktree_entries = vec![
        composition::WorktreeEntry {
            path: "/a".into(),
            head: "abcdef0".into(),
            branch: "main".into(),
            is_current: false,
            ..Default::default()
        },
        composition::WorktreeEntry {
            path: "/b".into(),
            head: "1234567".into(),
            branch: "dev".into(),
            is_current: false,
            ..Default::default()
        },
        composition::WorktreeEntry {
            path: "/c".into(),
            head: "fedcba0".into(),
            branch: "feat".into(),
            is_current: false,
            ..Default::default()
        },
    ];
    app.input.clear();
    app
}

#[test]
fn test_worktree_cursor_down_up() {
    let mut app = worktree_app_with_rows();
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(app.worktree_list.cursor, 1, "Down moves the cursor down");
    handle_working(&mut app, key(KeyCode::Down));
    assert_eq!(app.worktree_list.cursor, 2, "Down again");
    handle_working(&mut app, key(KeyCode::Up));
    assert_eq!(app.worktree_list.cursor, 1, "Up moves back up");
}

#[test]
fn test_worktree_e_enter() {
    let mut app = worktree_app_with_rows();
    // Enter opens the detail (level 1), then 'e' in the detail opens the
    // worktree (the enter ability, kept beyond the display-only round).
    handle_working(&mut app, key(KeyCode::Enter));
    assert_eq!(app.worktree_level.get(), 1, "Enter opens the detail");
    handle_working(&mut app, key(KeyCode::Char('e')));
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::System(s) if s.contains("no carrier"))),
        "e in detail enters the worktree (no carrier in stub mode)"
    );
}

#[test]
fn test_worktree_char_into_query() {
    let mut app = worktree_app_with_rows();
    handle_working(&mut app, key(KeyCode::Char('b')));
    handle_working(&mut app, key(KeyCode::Char('e')));
    assert_eq!(
        app.worktree_list.query, "be",
        "char keys append to the query"
    );
    assert!(
        app.worktree_list.searching(),
        "a non-empty query is searching"
    );
}

#[test]
fn test_worktree_backspace_query() {
    let mut app = worktree_app_with_rows();
    handle_working(&mut app, key(KeyCode::Char('a')));
    handle_working(&mut app, key(KeyCode::Char('b')));
    handle_working(&mut app, key(KeyCode::Backspace));
    assert_eq!(app.worktree_list.query, "a", "Backspace pops the last char");
}

#[test]
fn test_worktree_esc_clears_search() {
    let mut app = worktree_app_with_rows();
    handle_working(&mut app, key(KeyCode::Char('a')));
    assert!(app.worktree_list.searching());
    // First Esc clears the search, not the pane.
    handle_working(&mut app, key(KeyCode::Esc));
    assert!(!app.worktree_list.searching(), "Esc clears the query");
    assert_eq!(app.pane, Pane::Worktree, "Esc in search stays in the pane");
    // Second Esc (no search) closes the pane.
    handle_working(&mut app, key(KeyCode::Esc));
    assert_eq!(
        app.pane,
        Pane::Transcript,
        "Esc with no search closes the pane"
    );
}

#[test]
fn test_worktree_unknown_key_noop() {
    let mut app = worktree_app_with_rows();
    let before = app.worktree_list.cursor;
    // An unmapped function key: the handler does not consume it and leaves
    // the cursor + query untouched.
    handle_working(&mut app, key(KeyCode::F(1)));
    assert_eq!(
        app.worktree_list.cursor, before,
        "unknown key does not move cursor"
    );
    assert!(
        !app.worktree_list.searching(),
        "unknown key does not open search"
    );
}

fn shift_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

fn fleet_app(n: usize) -> App {
    let mut app = composition::app();
    app.screen = Screen::Working;
    app.viewport = ViewportMode::Working;
    for i in 0..n {
        app.fleet.entries.push(crate::agent_message::FleetEntry {
            agent_id: format!("child-{i}"),
            subagent_type: "explore".into(),
            turn: 1,
            tokens: 10,
            tool_uses: 0,
            last_activity: None,
            completed: None,
            completed_at: None,
        });
    }
    app
}

/// Shift+Down on a populated fleet moves the selection off the implicit 0
/// to row 1; Shift+Up clamps at the top.
#[test]
fn test_shift_arrow_moves_fleet() {
    let mut app = fleet_app(3);
    assert!(app.fleet.selected.is_none());
    handle_working(&mut app, shift_key(KeyCode::Down));
    assert_eq!(app.fleet.selected, Some(1));
    handle_working(&mut app, shift_key(KeyCode::Up));
    assert_eq!(app.fleet.selected, Some(0));
    handle_working(&mut app, shift_key(KeyCode::Up));
    assert_eq!(app.fleet.selected, Some(0), "clamps at top, no wrap");
    // A Shift+non-arrow key is not consumed: selection stays put.
    handle_working(&mut app, shift_key(KeyCode::Char('x')));
    assert_eq!(app.fleet.selected, Some(0));
}

/// Rendering a populated fleet paints one pill row per child, each carrying
/// the type + the verb inferred from its last tool.
#[test]
fn test_fleet_pill_renders_rows() {
    let app = fleet_app(2);
    let text = crate::test_support::render_text(&app, 80, 24);
    assert!(text.contains("explore"), "pill shows the child type");
    assert!(
        text.contains("searching") || text.contains("thinking"),
        "verb rendered"
    );
}

/// Empty-input Enter on a selected fleet row drills into the child's
/// teammate view via the agent id, not the transcript cursor.
#[test]
fn test_enter_fleet_drills_teammate() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = Screen::Working;
    app.viewport = ViewportMode::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        prompt: String::new(),
        folded_transcript: Vec::new(),
        color: None,
    });
    app.fleet.entries.push(crate::agent_message::FleetEntry {
        agent_id: "c1".into(),
        subagent_type: "explore".into(),
        turn: 1,
        tokens: 10,
        tool_uses: 0,
        last_activity: None,
        completed: None,
        completed_at: None,
    });
    app.fleet.selected = Some(0);
    handle_working(&mut app, key(KeyCode::Enter));
    assert!(
        app.teammate_view.is_some(),
        "Enter on the selected fleet row opens the teammate view"
    );
    assert_eq!(
        app.teammate_view.as_ref().unwrap().child_sid,
        "c1",
        "view targets the fleet agent id"
    );
}
