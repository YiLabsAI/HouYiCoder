//! Memory pane list, detail, filtering, controls, and command tests.

#![cfg(test)]

use houyicoder_protocol::frontend::SlashCommand;
use houyicoder_protocol::frontend::memory::ToggleState;

use crate::composition;
use crate::state::Pane;
use crate::test_support::render_text;

fn working() -> crate::state::App {
    let mut app = composition::app();
    app.screen = crate::state::Screen::Working;
    app
}

fn render(app: &crate::state::App) -> String {
    render_text(app, 100, 28)
}

/// List movement stays aligned with the filtered row selected on screen.
#[test]
fn test_memory_cursor_moves() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    // Cursor starts on row 0 (build-gate).
    let first = render(&app);
    assert!(first.contains("❯"), "cursor marker present");
    // Down to row 1 (comment-style).
    app.move_memory_cursor(1);
    let second = render(&app);
    assert!(
        second.contains("❯ [user] comment-style"),
        "cursor on comment-style:\n{second}"
    );
    // Up back to row 0.
    app.move_memory_cursor(-1);
    assert_eq!(app.memory.cursor(), 0, "cursor returns to 0");
    // Clamp: past the last row stays at the last.
    app.move_memory_cursor(100);
    assert_eq!(
        app.memory.cursor(),
        2,
        "cursor clamps to last row under All (3 entries)"
    );
    // Scope cycle resets the cursor to 0 (the filtered list changed).
    app.cycle_memory_scope();
    assert_eq!(app.memory.cursor(), 0, "scope cycle resets cursor");
}

/// Forget actions report the missing carrier in stub mode.
#[test]
fn test_memory_forget_no_carrier() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.move_memory_cursor(1);
    app.forget_memory_at_cursor();
    let d_out = render(&app);
    assert!(d_out.contains("no carrier"), "d action reports no carrier");
    // Command form: /memory forget <key>.
    app.run_tui_local_command("memory forget build-gate");
    let cmd_out = render(&app);
    assert!(cmd_out.contains("no carrier"), "command reports no carrier");
}

/// A refreshed MemoryList (the reply a forget / rescan sends) repopulates the
/// pane entries + resets the cursor so it never points past the new list.
#[test]
fn test_list_result_resets_cursor() {
    use crate::agent_message::AgentMessage;
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemorySummaryEntry;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.memory.set_cursor(5);
    let transcript_len = app.transcript.len();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    app.handle_agent_message(AgentMessage::MemoryListResult {
        req_id: RequestId(1),
        entries: vec![MemorySummaryEntry {
            key: "fresh-gate".into(),
            description: "re-seeded".into(),
            source: "project".into(),
            scope: "project".into(),
            mtime_secs: now,
        }],
    });
    assert_eq!(app.memory.cursor(), 0, "cursor reset on refresh");
    assert!(app.memory.entries().iter().any(|m| m.topic == "fresh-gate"));
    assert!(render(&app).contains("project · now"));
    assert_eq!(app.transcript.len(), transcript_len);
    assert_eq!(app.pane, crate::state::Pane::Memory);
}

/// Pane navigation and actions route to the selected memory.
#[test]
fn test_memory_pane_keys_route() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    assert_eq!(app.memory.cursor(), 0);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.memory.cursor(), 1, "Down moves cursor");
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.memory.cursor(), 0, "Up moves cursor back");
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        app.memory.scope(),
        crate::state::enums::MemoryScopeTab::User
    );
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    assert_eq!(app.memory.scope(), crate::state::enums::MemoryScopeTab::All);
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
    );
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
    );
    // d fires the forget action (stub mode reports no carrier).
    crate::keys::handle_working(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
    );
    let out = render(&app);
    assert!(out.contains("no carrier"), "d fires forget action");
}

#[test]
fn test_detail_soft_wrap_max() {
    let body = vec![ratatui::text::Line::from("x".repeat(25))];
    assert_eq!(crate::view::memory_pane::detail_max_offset(&body, 10, 2), 1);
}

#[test]
fn test_memory_detail_inline() {
    use crate::agent_message::AgentMessage;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemoryDetail;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let transcript_len = app.transcript.len();
    let req_id = RequestId(7);
    app.memory.request_detail(req_id, "newest".into());
    app.handle_agent_message(AgentMessage::MemoryShowResult {
        req_id,
        entry: Some(MemoryDetail {
            key: "newest".into(),
            content: format!(
                "full memory body\n{}",
                (0..40)
                    .map(|index| format!("detail line {index}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            source: "feedback".into(),
            description: "full memory body".into(),
            mtime_secs: 1,
        }),
    });

    let detail = render(&app);
    assert!(detail.contains("newest"));
    assert_eq!(detail.matches("full memory body").count(), 1);
    assert!(detail.contains("detail line 0"));
    assert!(detail.contains("Esc to back"));
    assert_eq!(app.transcript.len(), transcript_len);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.memory.detail_offset(), Some(1));
    for _ in 0..100 {
        crate::keys::handle_working(
            &mut app,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
        );
    }
    let bottom = app.memory.detail_offset().expect("detail open");
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.memory.detail_offset(), Some(bottom));

    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.memory.detail().is_none());
    assert_eq!(app.pane, crate::state::Pane::Memory);
}

#[test]
fn test_stale_detail_ignored() {
    use crate::agent_message::AgentMessage;
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemoryDetail;
    let mut app = working();
    app.pane = Pane::Memory;
    app.memory.request_detail(RequestId(1), "old".into());
    app.memory.request_detail(RequestId(2), "new".into());
    app.handle_agent_message(AgentMessage::MemoryShowResult {
        req_id: RequestId(1),
        entry: Some(MemoryDetail {
            key: "old".into(),
            content: "stale".into(),
            source: "user".into(),
            description: String::new(),
            mtime_secs: 0,
        }),
    });
    assert_eq!(app.memory.pending_key(), Some("new"));
    assert!(!render(&app).contains("stale"));
}

/// /memory search <term> narrows the list to entries whose key or description
/// match (composed with the scope tab). Esc clears the filter. The stub seeds
/// build-gate / comment-style / spec-driven — "build" matches only build-gate.
#[test]
fn test_memory_search_narrows_clears() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.run_tui_local_command("memory search build");
    let narrowed = render(&app);
    assert!(
        narrowed.contains("1 stored"),
        "search narrows to one:\n{narrowed}"
    );
    assert!(narrowed.contains("search: [build]"), "search query shown");
    assert!(narrowed.contains("build-gate"), "matching entry shows");
    assert!(!narrowed.contains("comment-style"), "non-match hidden");
    // Esc clears the filter, back to the full set.
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let cleared = render(&app);
    assert!(cleared.contains("3 stored"), "Esc restores full list");
    assert!(!cleared.contains("search: ["), "search row gone after Esc");
}

/// Enter requests detail for the selected row.
#[test]
fn test_enter_shows_no_carrier() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    crate::keys::handle_working(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let out = render(&app);
    assert!(out.contains("no carrier"), "enter fires show action");
}

#[test]
fn test_memory_pane_renders_rows() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let on = render(&app);
    assert_eq!(app.pane, Pane::Memory);
    // Footer verbs are static — the current state lives in the header row.
    assert!(
        on.contains("a to toggle auto-memory"),
        "auto-memory hint missing"
    );
    assert!(
        on.contains("c to toggle auto-dream"),
        "auto-dream hint missing"
    );
    // Both switches default on: the status row renders filled glyphs.
    assert!(on.contains("● auto-memory"), "on renders ●:\n{on}");
    assert!(on.contains("● auto-dream"), "on renders ●:\n{on}");
    app.memory.set_toggles(ToggleState {
        auto_memory: false,
        auto_dream: false,
    });
    let off = render(&app);
    assert!(off.contains("○ auto-memory"), "off renders ○:\n{off}");
    assert!(off.contains("○ auto-dream"), "off renders ○:\n{off}");
    assert!(
        off.contains("a to toggle auto-memory"),
        "footer verb does not flip with the state"
    );
}

/// The header color contract per cell: on = ● Cyan+BOLD, off = ○ DarkGray,
/// pending = ◌ Cyan. The whole item carries the style, glyph and label
/// alike, so a refactor cannot silently drop the coloring half.
#[test]
fn test_header_status_row_styles() {
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemoryToggleWhich;
    use ratatui::buffer::Buffer;
    use ratatui::style::{Color, Modifier, Style};

    /// The style of the cell where needle starts, scanning rows top-down.
    fn style_at(buf: &Buffer, needle: &str) -> Style {
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if let Some(idx) = row.find(needle) {
                let x = row[..idx].chars().count() as u16;
                return buf[(x, y)].style();
            }
        }
        panic!("needle {needle:?} not on screen");
    }

    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let buf = crate::test_support::render_buffer(&app, 100, 28);
    // Glyph cell and a label cell of the same item share the on-style.
    for needle in ["● auto-memory", "auto-memory"] {
        let st = style_at(&buf, needle);
        assert_eq!(st.fg, Some(Color::Cyan), "on item is Cyan: {needle}");
        assert!(
            st.add_modifier.contains(Modifier::BOLD),
            "on item is bold: {needle}"
        );
    }

    app.memory.set_toggles(ToggleState {
        auto_memory: false,
        auto_dream: false,
    });
    let buf = crate::test_support::render_buffer(&app, 100, 28);
    for needle in ["○ auto-memory", "auto-memory"] {
        let st = style_at(&buf, needle);
        assert_eq!(st.fg, Some(Color::DarkGray), "off item is dim: {needle}");
        assert!(
            !st.add_modifier.contains(Modifier::BOLD),
            "off item is not bold: {needle}"
        );
    }

    app.memory
        .begin_toggle(RequestId(1), MemoryToggleWhich::Dream);
    let buf = crate::test_support::render_buffer(&app, 100, 28);
    let st = style_at(&buf, "◌ auto-dream");
    assert_eq!(st.fg, Some(Color::Cyan), "pending item is Cyan");
    assert!(
        !st.add_modifier.contains(Modifier::BOLD),
        "pending is not bold — bold is reserved for confirmed on"
    );
}

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

#[test]
fn test_memory_toggle_bad_arg() {
    let mut app = working();
    let handled = app.run_tui_local_command("memory toggle bogus");
    assert!(handled, "bad toggle still handled");
    let out = render(&app);
    assert!(
        out.contains("/memory toggle auto|dream"),
        "usage names both switches:\n{out}"
    );
}

#[test]
fn test_memory_pane_empty_state() {
    let mut app = working();
    app.memory.clear_entries();
    app.run_command(SlashCommand::Memory);
    let out = render(&app);
    assert!(out.contains("0 stored"), "zero count missing:\n{out}");
    assert!(
        out.contains("no memories yet"),
        "empty hint missing:\n{out}"
    );
}

#[test]
fn test_memory_pane_renders_tag() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let out = render_text(&app, 100, 28);
    assert!(
        out.contains("[project] build-gate"),
        "scope and key missing:\n{out}"
    );
    assert!(
        out.contains("project · unknown"),
        "source and age missing:\n{out}"
    );
}

#[test]
fn test_scope_tab_narrows_list() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let all = render(&app);
    assert!(all.contains("3 stored"), "All shows all three:\n{all}");
    assert!(all.contains("[All]"), "All tab active");
    app.cycle_memory_scope();
    let user = render(&app);
    assert!(user.contains("1 stored"), "User narrows to one:\n{user}");
    assert!(user.contains("comment-style"), "user-scoped entry shows");
    assert!(
        !user.contains("build-gate"),
        "project-scoped hidden under User"
    );
    assert!(user.contains("[User]"), "User tab active");
    app.cycle_memory_scope();
    let project = render(&app);
    assert!(project.contains("build-gate"), "project-scoped shows");
    assert!(
        !project.contains("comment-style"),
        "user-scoped hidden under Project"
    );
    assert!(project.contains("[Project]"), "Project tab active");
    app.cycle_memory_scope();
    let auto = render(&app);
    assert!(auto.contains("spec-driven"), "auto-scoped shows");
    assert!(auto.contains("[Auto]"), "Auto tab active");
    app.cycle_memory_scope();
    let back = render(&app);
    assert!(back.contains("[All]"), "cycle wraps to All");
    assert!(back.contains("3 stored"), "All shows all three again");
}

/// A pending flip renders ◌ in the header, and the same switch cannot be
/// submitted twice while its flip is in flight; the other switch stays
/// operable.
#[test]
fn test_pending_blocks_repeat_toggle() {
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemoryToggleWhich;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    assert!(
        app.memory
            .begin_toggle(RequestId(1), MemoryToggleWhich::Auto)
    );
    assert!(
        !app.memory
            .begin_toggle(RequestId(2), MemoryToggleWhich::Auto),
        "a repeat press on the flipping switch is dropped"
    );
    assert!(
        app.memory
            .begin_toggle(RequestId(3), MemoryToggleWhich::Dream),
        "the other switch stays operable"
    );
    let out = render(&app);
    assert!(out.contains("◌ auto-memory"), "pending renders ◌:\n{out}");
    assert!(out.contains("◌ auto-dream"), "pending renders ◌:\n{out}");
}

/// A repeat forget of a key already in flight is dropped; a different key
/// stays operable, mirroring the toggle guard.
#[test]
fn test_forget_pending_blocks_repeat() {
    use houyicoder_protocol::envelope::RequestId;
    let mut app = working();
    assert!(app.memory.begin_forget(RequestId(1), "gate".into()));
    assert!(
        !app.memory.begin_forget(RequestId(2), "gate".into()),
        "a repeat forget of the same key is dropped"
    );
    assert!(
        app.memory.begin_forget(RequestId(3), "other".into()),
        "a different key stays operable"
    );
    assert_eq!(
        app.memory.pending_forget_keys(),
        vec!["gate".to_string(), "other".to_string()]
    );
}

/// A connection loss sweeps every in-flight pane mark: no reply can ever
/// land for them, so a sticky pending mark would refuse its switch forever.
/// A settled open detail is not pending and survives the sweep.
#[test]
fn test_connection_loss_clears_pending() {
    use crate::agent_message::AgentMessage;
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::{MemoryDetail, MemoryToggleWhich};
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.memory
        .begin_toggle(RequestId(1), MemoryToggleWhich::Auto);
    app.memory.request_detail(RequestId(2), "settled".into());
    app.memory.apply_detail(
        RequestId(2),
        Some(MemoryDetail {
            key: "settled".into(),
            content: "body".into(),
            source: "feedback".into(),
            description: "body".into(),
            mtime_secs: 1,
        }),
    );
    app.handle_agent_message(AgentMessage::ConnectionLost {
        message: "connection lost".into(),
    });
    assert_eq!(app.memory.pending_toggle_count(), 0, "toggle mark swept");
    assert!(
        app.memory.detail().is_some(),
        "an open detail is settled state and survives"
    );
    assert!(
        !render(&app).contains('◌'),
        "no pending mark left on screen:\n{}",
        render(&app)
    );
}

/// A loading detail is pending too — a connection loss sweeps it, so no
/// detail view sticks on a fetch that can never land.
#[test]
fn test_connection_loss_sweeps_detail() {
    use crate::agent_message::AgentMessage;
    use houyicoder_protocol::envelope::RequestId;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.memory.request_detail(RequestId(2), "loading".into());
    app.handle_agent_message(AgentMessage::ConnectionLost {
        message: "connection lost".into(),
    });
    assert!(app.memory.detail().is_none(), "loading detail swept");
}

/// An ordinary run failure keeps in-flight pane marks: the driver is still
/// alive, so their replies can still land.
#[test]
fn test_run_error_keeps_pending() {
    use crate::agent_message::AgentMessage;
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemoryToggleWhich;
    use houyicoder_protocol::frontend::run::RunError;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.memory
        .begin_toggle(RequestId(1), MemoryToggleWhich::Auto);
    app.handle_agent_message(AgentMessage::Done {
        result: Err(RunError {
            kind: "provider_exhausted".into(),
            message: "provider exhausted: timeout".into(),
        }),
    });
    assert!(
        app.memory.toggle_pending(MemoryToggleWhich::Auto),
        "the pending flip survives a run failure"
    );
}

/// A toggle pressed after the driver died must not register a pending flip:
/// the send fails synchronously, the registration rolls back, and the pane
/// writes a connection-lost failure line instead of waiting on a reply that
/// can never arrive.
#[test]
fn test_dead_driver_toggle_rollback() {
    use std::time::Duration;

    use crate::agent_message::AgentMessage;
    use crate::records::TranscriptLine;
    use crate::session::Session;
    use houyicoder_async::PFut;
    use houyicoder_client::Transport;
    use houyicoder_protocol::frontend::memory::MemoryToggleWhich;
    use houyicoder_protocol::wire::{WireError, WireErrorKind};

    /// A transport whose handshake fails immediately, so the driver exits
    /// (dropping the command receiver) before translating anything.
    struct FailOnConnect;
    impl Transport for FailOnConnect {
        fn send_frame(&mut self, _frame: &str) -> PFut<'_, Result<(), WireError>> {
            Box::pin(async { Ok(()) })
        }
        fn recv_frame(&mut self) -> PFut<'_, Result<Option<String>, WireError>> {
            Box::pin(async {
                Err(WireError::new(
                    WireErrorKind::Unavailable,
                    "no server",
                    false,
                ))
            })
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("runtime");
    let client = houyicoder_client::Client::new(Box::new(FailOnConnect));
    let (agent_tx, agent_rx) = std::sync::mpsc::channel::<AgentMessage>();
    let mut session = Session::spawn(client, agent_tx, agent_rx, &runtime);
    // Effect latch: the driver drops the command receiver before emitting
    // ConnectionLost, so once the event is observed the send below fails
    // deterministically — no sleep involved.
    let msg = session
        .poll_startup(Duration::from_secs(5))
        .expect("the dying driver emits ConnectionLost");
    assert!(
        matches!(msg, AgentMessage::ConnectionLost { .. }),
        "driver death is the typed event, got {msg:?}"
    );

    let mut app = working();
    app.session = Some(session);
    app.run_command(SlashCommand::Memory);
    app.toggle_memory_setting(MemoryToggleWhich::Auto);
    assert_eq!(
        app.memory.pending_toggle_count(),
        0,
        "the refused flip left no pending mark"
    );
    assert!(
        app.transcript.iter().any(|l| matches!(
            l,
            TranscriptLine::System(s)
                if s.contains("couldn't toggle auto-memory — connection lost")
        )),
        "the failure line names the switch and the cause"
    );
    // The switch stays operable: a repeat press is refused the same way
    // instead of being swallowed by a stuck pending mark.
    app.toggle_memory_setting(MemoryToggleWhich::Auto);
    assert_eq!(app.memory.pending_toggle_count(), 0);
    // Symmetry: the cursor actions roll back the same way.
    app.forget_memory_at_cursor();
    assert!(
        app.memory.pending_forget_keys().is_empty(),
        "the refused forget left no pending mark"
    );
    assert!(
        app.transcript.iter().any(|l| matches!(
            l,
            TranscriptLine::System(s) if s.contains("couldn't forget build-gate — connection lost")
        )),
        "the forget failure line names the key and the cause"
    );
    app.show_memory_at_cursor();
    assert!(
        app.memory.detail().is_none(),
        "the refused show left no loading detail"
    );
}

/// The matching toggle reply clears the pending mark, applies the new
/// state, and writes the confirmed outcome to the transcript.
#[test]
fn test_toggle_reply_writes_outcome() {
    use crate::agent_message::AgentMessage;
    use crate::records::TranscriptLine;
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemoryToggleWhich;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.memory
        .begin_toggle(RequestId(7), MemoryToggleWhich::Auto);
    app.handle_agent_message(AgentMessage::MemoryToggleStateResult {
        req_id: RequestId(7),
        state: ToggleState {
            auto_memory: false,
            auto_dream: true,
        },
    });
    assert!(
        app.transcript.iter().any(|l| matches!(
            l,
            TranscriptLine::System(s) if s.as_str() == "auto-memory off"
        )),
        "the outcome names the switch and its new state"
    );
    let out = render(&app);
    assert!(
        out.contains("○ auto-memory"),
        "pending cleared, off renders ○:\n{out}"
    );
}

/// A pane-open toggle-state read applies the snapshot but writes no
/// transcript — only a pending flip produces an outcome line.
#[test]
fn test_toggle_read_skips_transcript() {
    use crate::agent_message::AgentMessage;
    use houyicoder_protocol::envelope::RequestId;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let before = app.transcript.len();
    app.handle_agent_message(AgentMessage::MemoryToggleStateResult {
        req_id: RequestId(99),
        state: ToggleState {
            auto_memory: false,
            auto_dream: true,
        },
    });
    assert!(
        !app.memory.toggles().auto_memory,
        "the snapshot still applies"
    );
    assert_eq!(
        app.transcript.len(),
        before,
        "a plain state read writes no outcome"
    );
}

/// A forget's re-list writes the outcome only when the list reply matches
/// the registered pending action.
#[test]
fn test_forget_reply_writes_outcome() {
    use crate::agent_message::AgentMessage;
    use crate::records::TranscriptLine;
    use houyicoder_protocol::envelope::RequestId;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.memory
        .begin_forget(RequestId(8), "build-gate".to_string());
    app.handle_agent_message(AgentMessage::MemoryListResult {
        req_id: RequestId(8),
        entries: vec![],
    });
    assert!(
        app.transcript.iter().any(|l| matches!(
            l,
            TranscriptLine::System(s) if s.as_str() == "forgot build-gate"
        )),
        "the confirmed forget writes one outcome"
    );
    assert!(
        app.memory.pending_forget_keys().is_empty(),
        "the pending action cleared"
    );
}

/// A failed toggle names the action and the switch, keeps the header on
/// the old value, and clears the pending mark.
#[test]
fn test_toggle_failure_names_action() {
    use crate::agent_message::AgentMessage;
    use crate::records::TranscriptLine;
    use houyicoder_protocol::envelope::RequestId;
    use houyicoder_protocol::frontend::memory::MemoryToggleWhich;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.memory
        .begin_toggle(RequestId(9), MemoryToggleWhich::Auto);
    app.handle_agent_message(AgentMessage::RequestError {
        req_id: RequestId(9),
        message: "failed to save settings".to_string(),
    });
    assert!(
        app.transcript.iter().any(|l| matches!(
            l,
            TranscriptLine::System(s) if s.contains("couldn't toggle auto-memory")
                && s.contains("failed to save settings")
        )),
        "the failure names the action and the cause"
    );
    let out = render(&app);
    assert!(
        out.contains("● auto-memory"),
        "the header keeps the unchanged value:\n{out}"
    );
    assert!(!out.contains("◌"), "the pending mark cleared");
}

/// A failed forget names the key that could not be deleted.
#[test]
fn test_forget_failure_names_action() {
    use crate::agent_message::AgentMessage;
    use crate::records::TranscriptLine;
    use houyicoder_protocol::envelope::RequestId;
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    app.memory
        .begin_forget(RequestId(10), "build-gate".to_string());
    app.handle_agent_message(AgentMessage::RequestError {
        req_id: RequestId(10),
        message: "permission denied".to_string(),
    });
    assert!(
        app.transcript.iter().any(|l| matches!(
            l,
            TranscriptLine::System(s) if s.contains("couldn't forget build-gate")
                && s.contains("permission denied")
        )),
        "the failure names the key and the cause"
    );
}

/// The footer keeps toggles and scope on one line while they fit, and
/// moves the scope pair to a third line when the pane is too narrow.
#[test]
fn test_footer_splits_when_narrow() {
    let mut app = working();
    app.run_command(SlashCommand::Memory);
    let wide = render_text(&app, 100, 28);
    assert!(
        wide.contains("toggle auto-dream · Tab/Left/Right to switch scope"),
        "wide keeps the toggles and scope on one line:\n{wide}"
    );
    let narrow = render_text(&app, 70, 28);
    assert!(
        !narrow.contains("toggle auto-dream · Tab/Left/Right"),
        "narrow moves the scope pair to its own line:\n{narrow}"
    );
    assert!(
        narrow.contains("Tab/Left/Right to switch scope"),
        "the scope hint is still present:\n{narrow}"
    );
}
