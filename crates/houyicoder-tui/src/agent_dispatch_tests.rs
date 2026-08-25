use crate::agent_message::AgentMessage;
use crate::state::Pane;
use crate::transcript::TranscriptFrame;
use houyicoder_protocol::frontend::session_update::{
    SessionUpdate, ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};

fn tool_call_frame(id: &str, title: &str, status: ToolCallStatus) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new(id, title).status(status),
    ))
}

fn tool_done_frame(id: &str) -> TranscriptFrame {
    TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        id,
        ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
    )))
}

#[test]
fn test_frame_log_call_id() {
    let f = tool_call_frame("c1", "glob", ToolCallStatus::InProgress);
    let msg = super::frame_log_msg(&f).expect("call frame logged");
    assert!(msg.contains("id=c1"), "id in {msg}");
    assert!(msg.contains("tool=glob"), "tool in {msg}");
}

#[test]
fn test_frame_log_result_shape() {
    // A diff-bearing result (edit) tags "diff"; a glob result tags "files";
    // a status-only update tags "status". These tags let a debug log reveal
    // a swapped call_id at the server or a TUI pairing bug at a glance.
    let diff = TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "c1",
        ToolCallUpdateFields::new()
            .raw_output(serde_json::json!({"diff": "@@ -1 +1 @@\n-a\n+b\n"})),
    )));
    let files = TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "c2",
        ToolCallUpdateFields::new().raw_output(serde_json::json!({"num_files": 0})),
    )));
    let status = tool_done_frame("c3");
    assert!(
        super::frame_log_msg(&diff).unwrap().contains("shape=diff"),
        "diff result tags diff"
    );
    assert!(
        super::frame_log_msg(&files)
            .unwrap()
            .contains("shape=files"),
        "glob result tags files"
    );
    assert!(
        super::frame_log_msg(&status)
            .unwrap()
            .contains("shape=status"),
        "status-only tags status"
    );
}

fn done_msg() -> AgentMessage {
    AgentMessage::Done {
        result: Ok(houyicoder_protocol::frontend::run::RunResult {
            outcome: houyicoder_protocol::frontend::run::RunOutcome::FinalOutput {
                content: vec![houyicoder_protocol::frontend::run::ContentBlock::Text {
                    text: "ok".into(),
                }],
            },
            usage: houyicoder_protocol::llm::Usage::default(),
            turns: 1,
            stop_reason: houyicoder_protocol::frontend::run::StopReason::EndTurn,
        }),
    }
}

fn todo_frame(items: &[(&str, &str)]) -> TranscriptFrame {
    let todos: Vec<serde_json::Value> = items
        .iter()
        .map(|(content, status)| serde_json::json!({"content": content, "status": status}))
        .collect();
    TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new("todo_1", "todo_write").raw_input(serde_json::json!({"todos": todos})),
    ))
}

/// A TrajectoryResult with redundant calls renders the redundant section
/// (same-message repeat / cross-turn context-loss re-read) as a system
/// line. Pins the dispatch side of the trajectory redundant surfacing.
#[test]
fn test_trajectory_renders_redundant_section() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::TrajectoryResult {
        entries: vec![],
        redundant: vec![
            houyicoder_protocol::frontend::trajectory::RedundantCallEntry {
                tool: "read".into(),
                input_preview: "{\"file_path\":\"a.rs\"}".into(),
                kind: "same-batch".into(),
                gap: 0,
                last_seq: 3,
            },
        ],
    });
    assert!(
        app.transcript.iter().any(|l| matches!(
            l,
            TranscriptLine::System(s) if s.contains("same-message repeat")
        )),
        "redundant section rendered"
    );
}

#[test]
fn test_tool_frames_track_set() {
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::Frame(tool_call_frame(
        "call_1",
        "bash",
        ToolCallStatus::InProgress,
    )));
    assert!(app.running_tools.contains("call_1"));
    app.handle_agent_message(AgentMessage::Frame(tool_done_frame("call_1")));
    assert!(app.running_tools.is_empty());
}

#[test]
fn test_done_clears_running_tools() {
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::Frame(tool_call_frame(
        "call_1",
        "bash",
        ToolCallStatus::InProgress,
    )));
    app.handle_agent_message(done_msg());
    assert!(app.running_tools.is_empty());
}

#[test]
fn test_initial_projection_skips_stamps() {
    // A resumed session's first projection contains historic completed
    // items: they must not be stamped as recently completed.
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::Frame(todo_frame(&[
        ("old work", "completed"),
        ("current", "in_progress"),
    ])));
    app.handle_agent_message(done_msg());
    assert!(app.todo_completion_at.is_empty());
    // A subsequent projection completing a new item stamps it.
    app.handle_agent_message(AgentMessage::Frame(todo_frame(&[
        ("old work", "completed"),
        ("current", "completed"),
    ])));
    app.handle_agent_message(done_msg());
    assert!(app.todo_completion_at.contains_key("current"));
    assert!(!app.todo_completion_at.contains_key("old work"));
}

/// The toggle-state result (a read on pane-open or after a flip) applies
/// the snapshot to the view state. The pane reopens ONLY when the user is
/// still on it — a late flip response arriving after the user dismissed the
/// pane must not yank them back. Pins both the state-apply wiring + the
/// dismissal-respect guard so a later refactor cannot drop either.
#[test]
fn test_toggle_state_applies_view() {
    use houyicoder_protocol::frontend::memory::ToggleState;
    // On the pane: the snapshot applies + the pane stays open.
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Memory;
    app.handle_agent_message(AgentMessage::MemoryToggleStateResult {
        state: ToggleState {
            auto_memory: false,
            auto_dream: true,
        },
    });
    assert!(!app.memory_toggles.auto_memory, "auto-memory applied");
    assert!(app.memory_toggles.auto_dream, "auto-dream applied");
    assert_eq!(app.pane, crate::state::Pane::Memory, "pane stays open");
    // Dismissed (pane moved away): the snapshot still applies, but the
    // pane is NOT yanked back to Memory.
    let mut app = crate::composition::app();
    app.pane = crate::state::Pane::Spec;
    app.handle_agent_message(AgentMessage::MemoryToggleStateResult {
        state: ToggleState {
            auto_memory: false,
            auto_dream: true,
        },
    });
    assert!(
        !app.memory_toggles.auto_memory,
        "auto-memory applied on dismissal"
    );
    assert_eq!(
        app.pane,
        crate::state::Pane::Spec,
        "late result does not yank back a dismissed pane"
    );
}

/// A list refresh (MemoryListResult) reopens the pane ONLY when the user
/// is still on it. A late list response arriving after the user dismissed
/// the pane must not yank them back — the data still lands (the next
/// /memory open reads it). Pins the dismissal-respect guard on the list
/// path so a later refactor cannot drop it.
#[test]
fn test_memory_list_respects_dismissal() {
    use houyicoder_protocol::frontend::memory::MemorySummaryEntry;
    // On the pane: the list populates + the pane stays open.
    let mut app = crate::composition::app();
    app.pane = Pane::Memory;
    app.handle_agent_message(AgentMessage::MemoryListResult {
        entries: vec![MemorySummaryEntry {
            key: "build-gate".to_string(),
            description: "make check stays green".to_string(),
            source: "project".to_string(),
            scope: "project".to_string(),
            mtime_secs: 0,
        }],
    });
    assert_eq!(app.pane, Pane::Memory, "pane stays open on active refresh");
    assert!(
        app.memory_entries.iter().any(|e| e.topic == "build-gate"),
        "list entry populated"
    );
    // Dismissed (pane moved away): the data still lands, but the pane is
    // NOT yanked back to Memory.
    let mut app = crate::composition::app();
    app.pane = Pane::Spec;
    app.handle_agent_message(AgentMessage::MemoryListResult {
        entries: vec![MemorySummaryEntry {
            key: "build-gate".to_string(),
            description: "make check stays green".to_string(),
            source: "project".to_string(),
            scope: "project".to_string(),
            mtime_secs: 0,
        }],
    });
    assert_eq!(
        app.pane,
        Pane::Spec,
        "late list does not yank back a dismissed pane"
    );
    assert!(
        app.memory_entries.iter().any(|e| e.topic == "build-gate"),
        "list data still lands on dismissal"
    );
}

/// A background memory-saved event renders one system line with the verb
/// the kind maps to (extract = Saved, dream = Improved) + a singular or
/// plural noun. Pins the render wiring so a later refactor cannot drop it.
#[test]
fn test_memory_saved_renders_notice() {
    use crate::state::Screen;
    use houyicoder_protocol::frontend::memory::MemorySavedKind;
    let mut app = crate::composition::app();
    app.screen = Screen::Working;

    // Extracted, plural: "Saved 3 memories".
    app.handle_agent_message(AgentMessage::MemorySaved {
        count: 3,
        kind: MemorySavedKind::Extracted,
    });
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("Saved 3 memories"),
        "extract verb + plural noun should render: {out}"
    );

    // Consolidated, singular: "Improved 1 memory".
    app.handle_agent_message(AgentMessage::MemorySaved {
        count: 1,
        kind: MemorySavedKind::Consolidated,
    });
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("Improved 1 memory"),
        "dream verb + singular noun should render: {out}"
    );
}

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

#[test]
fn test_agents_result_stores_directory() {
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::AgentsResult {
        directory: "## Available agents\n\n- explore: fast".into(),
    });
    assert_eq!(
        app.agent_directory.as_deref(),
        Some("## Available agents\n\n- explore: fast"),
    );
}

#[test]
fn test_tool_list_result_stored() {
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::ToolListResult {
        tools: vec![houyicoder_protocol::frontend::tools::ToolEntry {
            name: "bash".into(),
            description: "run a command".into(),
        }],
    });
    assert_eq!(app.tool_entries.len(), 1);
    assert_eq!(app.tool_entries[0].name, "bash");
}

/// The /tools pane renders the registered tool list sorted by name with each
/// row's first description line. A populated list never shows the empty
/// placeholder. Pins the render so a refactor that drops sorting or the row
/// format fails here.
#[test]
fn test_tools_pane_renders_entries() {
    use houyicoder_protocol::frontend::tools::ToolEntry;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.pane = crate::state::Pane::Tools;
    app.tool_entries = vec![
        ToolEntry {
            name: "zed".into(),
            description: "edits files".into(),
        },
        ToolEntry {
            name: "bash".into(),
            description: "runs a command\nsecond line".into(),
        },
    ];
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("bash"), "bash row renders: {out}");
    assert!(out.contains("zed"), "zed row renders: {out}");
    // Sorted: bash before zed.
    assert!(
        out.find("bash:").unwrap() < out.find("zed:").unwrap(),
        "tools sorted by name"
    );
    // Only the first description line shows (the second line is not rendered).
    assert!(
        !out.contains("second line"),
        "only the first description line renders: {out}"
    );
}

/// An empty /tools list renders the placeholder, not a blank pane. Pins the
/// empty-branch render so a refactor that drops the guard renders blank.
#[test]
fn test_tools_pane_renders_empty() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.pane = crate::state::Pane::Tools;
    app.tool_entries = vec![];
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("(no tools loaded)"),
        "empty tools renders placeholder: {out}"
    );
}

/// When the fleet is populated (child agents running), the /agents pane
/// lists each agent with name, role, and state. v0 has no live fleet, so this
/// pins the fleet-render contract child-tracking will drive. The directory
/// branch is exercised separately by the stub /agents render.
#[test]
fn test_agents_pane_renders_fleet() {
    use crate::evidence::AgentStatus;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.pane = crate::state::Pane::Agents;
    app.agent_directory = None;
    app.agents = vec![
        AgentStatus {
            name: "explore".into(),
            role: "search".into(),
            state: "idle".into(),
        },
        AgentStatus {
            name: "build".into(),
            role: "implement".into(),
            state: "running".into(),
        },
    ];
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("explore"), "fleet name renders: {out}");
    assert!(out.contains("search"), "fleet role renders: {out}");
    assert!(out.contains("running"), "fleet state renders: {out}");
}

/// The agent directory is a multi-line string (header + bullets). Each source
/// line must render on its own terminal row; a single Line::from would
/// flatten newlines into same-row spans. Pins the per-line split so a
/// refactor that re-flattens fails here.
#[test]
fn test_agents_directory_renders_lines() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.pane = crate::state::Pane::Agents;
    app.agent_directory =
        Some("## Available agents\n\n- explore: fast search\n- plan: design".into());
    let out = crate::test_support::render_text(&app, 80, 24);
    let header_row = out.lines().position(|l| l.contains("Available agents"));
    let explore_row = out.lines().position(|l| l.contains("explore"));
    assert!(header_row.is_some(), "header renders: {out}");
    assert!(explore_row.is_some(), "explore renders: {out}");
    assert!(
        header_row.unwrap() < explore_row.unwrap(),
        "header must sit above the explore bullet (per-line render), not flatten to one row"
    );
}

/// A Some("") reply (no agents registered) renders the placeholder, not a
/// blank pane. Pins the empty-filter so the fallback fires on empty content.
#[test]
fn test_agents_directory_empty_placeholder() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.pane = crate::state::Pane::Agents;
    app.agent_directory = Some(String::new());
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("(no agent directory loaded)"),
        "empty directory shows placeholder, not blank: {out}"
    );
}

/// A Subagent delegation line renders inline in the parent flow (no context
/// switch): collapsed shows the subagent type + summary + an expand hint.
/// Pins the inline-fold render so a refactor that drops the Subagent arm (or
/// reverts to a context-switch) fails here.
#[test]
fn test_subagent_renders_collapsed() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth module".into(),
        folded_transcript: Vec::new(),
    });
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("explore"), "subagent type renders: {out}");
    assert!(out.contains("found auth module"), "summary renders: {out}");
    assert!(
        out.contains("ctrl+o to expand"),
        "collapsed shows the expand hint: {out}"
    );
}

/// When the child_sid is in expanded_subagents, the Subagent line renders the
/// collapse hint + a placeholder for the unloaded child transcript. The
/// fetch that fills folded_transcript lands next; this pins the expanded
/// branch so a refactor that drops it fails here.
#[test]
fn test_subagent_renders_expanded() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth module".into(),
        folded_transcript: Vec::new(),
    });
    app.expanded_subagents.insert("child-1".into());
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("ctrl+o to collapse"),
        "expanded shows the collapse hint: {out}"
    );
    assert!(
        out.contains("child transcript not yet loaded"),
        "expanded shows the placeholder until the fetch lands: {out}"
    );
}

/// When expanded and the child transcript is loaded, the Subagent line
/// renders the child's rows inline (recursively through the same row
/// builder). Pins the recursive render branch so a refactor that drops it
/// fails here.
#[test]
fn test_subagent_expanded_renders_child() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth module".into(),
        folded_transcript: vec![TranscriptLine::Agent("child reply: auth is here".into())],
    });
    app.expanded_subagents.insert("child-1".into());
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(
        out.contains("child reply: auth is here"),
        "expanded with a loaded child renders the child row inline: {out}"
    );
}

/// Ctrl+O toggles the last Subagent delegation's expand state. The first
/// call expands (expanded_subagents gains the child_sid); the second
/// collapses (removed). Pins the toggle wiring so a refactor that drops it
/// fails here.
#[test]
fn test_subagent_toggle_expand() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        folded_transcript: Vec::new(),
    });
    assert!(app.expanded_subagents.is_empty(), "starts collapsed");
    assert!(app.toggle_subagent_expand(), "toggle returns true");
    assert!(
        app.expanded_subagents.contains("child-1"),
        "first toggle expands"
    );
    assert!(app.toggle_subagent_expand(), "toggle returns true again");
    assert!(app.expanded_subagents.is_empty(), "second toggle collapses");
}

/// When no Subagent is present, Ctrl+O falls through to the ThoughtFor
/// expand path. Pins the fallthrough so a refactor that drops it fails.
#[test]
fn test_ctrl_o_fallthrough() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::ThoughtFor {
        secs: 3,
        reasoning: Some("pondered the task".into()),
        tool_summary: None,
        turn_id: "t1".into(),
    });
    assert!(app.expanded_thinking.is_empty());
    crate::keys::handle_ctrl_o(&mut app);
    assert!(
        app.expanded_thinking.contains("t1"),
        "falls through to ThoughtFor when no Subagent is present"
    );
}

/// With no Subagent and no ThoughtFor but an active todo list, Ctrl+O
/// expands the collapsed checklist. Pins the todo fallthrough path.
#[test]
fn test_ctrl_o_todo_expand() {
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.todos_cache.push(crate::todo_view::TodoView {
        content: "do the thing".into(),
        status: crate::todo_view::TodoStatus::Pending,
        active_form: None,
    });
    assert!(!app.todo_expanded);
    crate::keys::handle_ctrl_o(&mut app);
    assert!(app.todo_expanded, "Ctrl+O expands the todo list");
}

/// Expanding/collapsing a Subagent fold does not shift the content row
/// indices of sibling transcript lines. The child transcript renders as
/// display rows inside the fold, not as new content rows in the parent
/// index space, so the parent's line indices stay stable (the single
/// source of truth). Pins the invariant so a refactor that reindexes on
/// expand fails here.
#[test]
fn test_subagent_expand_stable_index() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.transcript.push(TranscriptLine::User("task".into()));
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "child-1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
    });
    app.transcript
        .push(TranscriptLine::Agent("parent result".into()));
    // Content row indices: User=0, Subagent=1, Agent=2.
    assert!(matches!(app.transcript[0], TranscriptLine::User(_)));
    assert!(matches!(app.transcript[1], TranscriptLine::Subagent { .. }));
    assert!(matches!(app.transcript[2], TranscriptLine::Agent(_)));
    // Expand the Subagent — the transcript Vec does not change.
    app.expanded_subagents.insert("child-1".into());
    assert!(matches!(app.transcript[0], TranscriptLine::User(_)));
    assert!(matches!(app.transcript[1], TranscriptLine::Subagent { .. }));
    assert!(matches!(app.transcript[2], TranscriptLine::Agent(_)));
    // Collapse — still stable.
    app.expanded_subagents.remove("child-1");
    assert!(matches!(app.transcript[0], TranscriptLine::User(_)));
    assert!(matches!(app.transcript[1], TranscriptLine::Subagent { .. }));
    assert!(matches!(app.transcript[2], TranscriptLine::Agent(_)));
}

/// line_display_rows must match the render row count for a Subagent:
/// 1 when collapsed, 2 when expanded with empty folded_transcript
/// (head + placeholder), and 1 + child rows when expanded with loaded
/// children. Pins the count==render invariant the scroll math depends on.
#[test]
fn test_subagent_row_count() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    let sub = TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
    };
    // Collapsed: 1 head row.
    assert_eq!(app.line_display_rows(&sub), 1);
    // Expanded with loaded children: 1 head + child rows.
    app.expanded_subagents.insert("c1".into());
    let child_rows = app.line_display_rows(&TranscriptLine::Agent("child reply".into()));
    assert_eq!(app.line_display_rows(&sub), 1 + child_rows);
    // Expanded with empty folded_transcript: 1 head + 1 placeholder.
    let empty_sub = TranscriptLine::Subagent {
        child_sid: "c2".into(),
        subagent_type: "explore".into(),
        summary: "no output".into(),
        folded_transcript: Vec::new(),
    };
    app.expanded_subagents.insert("c2".into());
    assert_eq!(app.line_display_rows(&empty_sub), 2);
}

/// A ChildTranscriptResult reply populates the matching Subagent line's
/// folded_transcript through the same projection as the parent flow
/// (isomorphism: the child renders as TranscriptLines, not an opaque blob).
/// Pins the fill arm + the transcript_from_frames projection so a refactor
/// that drops the in-place swap or the projection fails here.
#[test]
fn test_child_transcript_fills_folded() {
    use crate::records::TranscriptLine;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        folded_transcript: Vec::new(),
    });
    let frames = vec![
        TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text {
                text: "find auth".into(),
            },
        ))),
        TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text {
                text: "auth is in src/auth".into(),
            },
        ))),
    ];
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames,
    });
    match &app.transcript[0] {
        TranscriptLine::Subagent {
            folded_transcript, ..
        } => {
            assert_eq!(
                folded_transcript.len(),
                2,
                "child frames projected to 2 lines"
            );
            assert!(matches!(folded_transcript[0], TranscriptLine::User(_)));
            assert!(matches!(folded_transcript[1], TranscriptLine::Agent(_)));
        }
        other => panic!("subagent line preserved, got {other:?}"),
    }
}

/// An empty frame list (child log missing/unreadable, or the child produced
/// no durable events) surfaces an explicit unavailable line rather than
/// re-showing the placeholder, so a re-expand does not refetch forever.
/// Pins the empty-case guard.
#[test]
fn test_child_transcript_empty_unavailable() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        folded_transcript: Vec::new(),
    });
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: Vec::new(),
    });
    match &app.transcript[0] {
        TranscriptLine::Subagent {
            folded_transcript, ..
        } => {
            assert_eq!(folded_transcript.len(), 1, "empty -> one unavailable line");
            assert!(
                matches!(&folded_transcript[0], TranscriptLine::System(s) if s.contains("unavailable")),
                "unavailable line surfaces, got {:?}",
                folded_transcript[0]
            );
        }
        other => panic!("subagent line preserved, got {other:?}"),
    }
}

/// The in-place swap preserves the Subagent line's position when trailing
/// lines exist (remove + insert at the same index, not push to tail). Pins
/// position stability so an expanded fold does not yank trailing content.
#[test]
fn test_child_transcript_preserves_position() {
    use crate::records::TranscriptLine;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Agent("before".into()));
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        folded_transcript: Vec::new(),
    });
    app.transcript.push(TranscriptLine::Agent("after".into()));
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: vec![TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "child reply".into(),
            }),
        ))],
    });
    assert!(
        matches!(app.transcript[0], TranscriptLine::Agent(_)),
        "before stays at 0"
    );
    assert!(
        matches!(app.transcript[1], TranscriptLine::Subagent { .. }),
        "subagent stays at 1"
    );
    assert!(
        matches!(app.transcript[2], TranscriptLine::Agent(_)),
        "after stays at 2"
    );
}

/// Collapse does not clear an already-loaded folded_transcript, so a
/// re-expand reuses the cached rows without refetching. Pins the retain
/// semantics (the fetch is first-expand-only).
#[test]
fn test_subagent_collapse_keeps_folded() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
    });
    // Expand, then collapse: the child rows survive the collapse.
    app.toggle_subagent_expand();
    assert!(app.expanded_subagents.contains("c1"));
    app.toggle_subagent_expand();
    assert!(!app.expanded_subagents.contains("c1"), "collapsed");
    match &app.transcript[0] {
        TranscriptLine::Subagent {
            folded_transcript, ..
        } => {
            assert_eq!(folded_transcript.len(), 1, "collapse kept the child rows");
            assert!(matches!(folded_transcript[0], TranscriptLine::Agent(_)));
        }
        other => panic!("subagent line preserved, got {other:?}"),
    }
}

/// Cursor targeting: when the cursor is on a specific Subagent line, Ctrl+O
/// expands that line, not the last one. Without a cursor, falls back to the
/// last Subagent. Pins the cursor walk's spacer logic against the flat
/// content-row space the selection lives in.
#[test]
fn test_subagent_cursor_targeting() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
    });
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c2".into(),
        subagent_type: "plan".into(),
        summary: "second".into(),
        folded_transcript: vec![TranscriptLine::Agent("child reply 2".into())],
    });
    app.selection.start(0, 0);
    app.toggle_subagent_expand();
    assert!(
        app.expanded_subagents.contains("c1"),
        "cursor on first line expands it"
    );
    assert!(
        !app.expanded_subagents.contains("c2"),
        "the second line is not expanded when the cursor targets the first"
    );
    app.expanded_subagents.clear();
    app.selection.anchor = None;
    app.toggle_subagent_expand();
    assert!(
        app.expanded_subagents.contains("c2"),
        "no cursor falls back to the last Subagent"
    );
}

/// A fetched wire child frame converts to the live-frame shape the
/// transcript projection consumes, so child rows render through the same
/// pipeline as the parent flow. Pins the From impl at the driver boundary.
#[test]
fn test_child_frame_converts() {
    use houyicoder_protocol::envelope::ChildTranscriptFrame;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::{ContentChunk, SessionUpdate};
    let wire = ChildTranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
        ContentBlock::Text {
            text: "child reply".into(),
        },
    )));
    let frame: TranscriptFrame = wire.into();
    assert!(
        matches!(
            frame,
            TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(_))
        ),
        "session frame converts: {frame:?}"
    );
    let wire_acpx = ChildTranscriptFrame::Acpx(houyicoder_protocol::acpx::AcpxNotification::new(
        houyicoder_protocol::acpx::AcpxMethod::ToolProgress,
        serde_json::Value::Null,
    ));
    let acpx: TranscriptFrame = wire_acpx.into();
    assert!(
        matches!(acpx, TranscriptFrame::Acpx(_)),
        "acpx frame converts: {acpx:?}"
    );
}

/// A parent transcript rebuild must not wipe the fetched child rows from a
/// Subagent line. The line is frame-derived, so a rebuild re-projects it
/// empty; the merge carries the old folded_transcript over when the
/// child_sid matches. Pins the retain semantics the rebuild otherwise
/// violates.
#[test]
fn test_subagent_folded_survives_rebuild() {
    use crate::records::TranscriptLine;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;
    let call = TranscriptFrame::Session(SessionUpdate::ToolCall(
        ToolCall::new("ag1", "agent")
            .status(ToolCallStatus::Completed)
            .raw_input(serde_json::json!({"subagent_type": "explore"})),
    ));
    let result = TranscriptFrame::Session(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
        "ag1",
        ToolCallUpdateFields::new()
            .raw_output(serde_json::json!({"agentId": "c1", "content": "found auth"})),
    )));
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::Frame(call));
    app.handle_agent_message(AgentMessage::Frame(result));
    assert!(
        app.transcript
            .iter()
            .any(|l| matches!(l, TranscriptLine::Subagent { .. })),
        "subagent line projected from the agent-tool result"
    );
    // Fill folded_transcript (the on-expand fetch landing).
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames: vec![TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "child reply".into(),
            }),
        ))],
    });
    // A parent rebuild re-projects frames; the fetched child rows survive.
    app.rebuild_transcript();
    match app
        .transcript
        .iter()
        .find(|l| matches!(l, TranscriptLine::Subagent { .. }))
    {
        Some(TranscriptLine::Subagent {
            child_sid,
            folded_transcript,
            ..
        }) => {
            assert_eq!(child_sid, "c1");
            assert_eq!(
                folded_transcript.len(),
                1,
                "fetched child rows survived the rebuild"
            );
            assert!(matches!(folded_transcript[0], TranscriptLine::Agent(_)));
        }
        other => panic!("subagent line survived, got {other:?}"),
    }
}

/// Enter on a Subagent line opens the teammate view targeting the line at
/// the cursor, not the last one. The view carries the subagent_type +
/// summary for the banner. Pins the cursor-targeting entry so a refactor
/// that opens the wrong child fails here.
#[test]
fn test_enter_teammate_targets_cursor() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
    });
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c2".into(),
        subagent_type: "plan".into(),
        summary: "second".into(),
        folded_transcript: vec![TranscriptLine::Agent("child reply 2".into())],
    });
    app.selection.start(0, 0);
    assert!(
        app.enter_teammate_view(),
        "cursor on a Subagent line enters"
    );
    let view = app.teammate_view.as_ref().expect("teammate view is open");
    assert_eq!(view.child_sid, "c1", "cursor targets the first line");
    assert_eq!(view.subagent_type, "explore");
    assert_eq!(view.summary, "first");
    // A pre-loaded fold copies into the view so it renders immediately.
    assert_eq!(view.transcript.len(), 1);
    assert!(matches!(view.transcript[0], TranscriptLine::Agent(_)));
    // active_transcript swaps to the child's, not the parent's.
    assert_eq!(
        app.active_transcript().len(),
        1,
        "active transcript is the child's"
    );
}

/// Esc clears the teammate view, returning active_transcript to the parent.
#[test]
fn test_exit_teammate_clears_view() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Agent("parent".into()));
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        folded_transcript: vec![TranscriptLine::Agent("child reply".into())],
    });
    assert!(app.enter_teammate_view());
    assert!(app.teammate_view.is_some());
    assert_eq!(app.active_transcript().len(), 1, "viewing child");
    app.exit_teammate_view();
    assert!(app.teammate_view.is_none(), "view cleared on exit");
    assert_eq!(
        app.active_transcript().len(),
        2,
        "parent transcript restored"
    );
}

/// A fetched child frame fills the teammate view's transcript through the
/// same projection the inline fold receives, so the drilled-in view is
/// isomorphic with the expanded fold, not a parallel simplification. Pins
/// the T36e isomorphism contract: child view renders via the main pipeline.
#[test]
fn test_teammate_view_fill_isomorphic() {
    use crate::records::TranscriptLine;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;
    use houyicoder_protocol::frontend::session_update::SessionUpdate;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "found auth".into(),
        folded_transcript: Vec::new(),
    });
    // Enter with an unloaded fold: the view opens empty and the fetch fires.
    assert!(app.enter_teammate_view());
    assert_eq!(
        app.teammate_view.as_ref().unwrap().transcript.len(),
        0,
        "view empty until fetch lands"
    );
    let frames = vec![
        TranscriptFrame::Session(SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text {
                text: "find auth".into(),
            },
        ))),
        TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text {
                text: "auth is in src/auth".into(),
            },
        ))),
    ];
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c1".into(),
        frames,
    });
    let view = app.teammate_view.as_ref().unwrap();
    assert_eq!(view.transcript.len(), 2, "view filled by the fetch");
    assert!(matches!(view.transcript[0], TranscriptLine::User(_)));
    assert!(matches!(view.transcript[1], TranscriptLine::Agent(_)));
    // Isomorphism: the fold-group and the view carry the same projected rows.
    let folded = match &app.transcript[0] {
        TranscriptLine::Subagent {
            folded_transcript, ..
        } => folded_transcript.clone(),
        other => panic!("subagent line preserved, got {other:?}"),
    };
    assert_eq!(
        view.transcript.len(),
        folded.len(),
        "view and fold hold the same row count"
    );
    assert_eq!(
        view.transcript
            .iter()
            .map(|l| l.render())
            .collect::<Vec<_>>(),
        folded.iter().map(|l| l.render()).collect::<Vec<_>>(),
        "view and fold render identically"
    );
}

/// A fetch landing for a child the user is NOT viewing must not clobber the
/// view's transcript. Pins the child_sid match guard.
#[test]
fn test_other_child_keeps_view() {
    use crate::records::TranscriptLine;
    use houyicoder_protocol::frontend::run::ContentBlock;
    use houyicoder_protocol::frontend::session_update::ContentChunk;
    use houyicoder_protocol::frontend::session_update::SessionUpdate;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        folded_transcript: vec![TranscriptLine::Agent("viewed child".into())],
    });
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c2".into(),
        subagent_type: "plan".into(),
        summary: "second".into(),
        folded_transcript: Vec::new(),
    });
    app.selection.start(0, 0);
    assert!(app.enter_teammate_view());
    assert_eq!(app.teammate_view.as_ref().unwrap().child_sid, "c1");
    // A fetch for c2 arrives while viewing c1.
    app.handle_agent_message(AgentMessage::ChildTranscriptResult {
        child_sid: "c2".into(),
        frames: vec![TranscriptFrame::Session(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text {
                text: "other child".into(),
            }),
        ))],
    });
    let view = app.teammate_view.as_ref().unwrap();
    assert_eq!(view.child_sid, "c1", "view unchanged");
    assert_eq!(view.transcript.len(), 1, "view transcript not clobbered");
    assert!(
        matches!(view.transcript[0], TranscriptLine::Agent(ref s) if s == "viewed child"),
        "the viewed child's rows are intact"
    );
}

/// Enter on a transcript with no Subagent line returns false and opens no
/// view, so the caller falls through to submit. Pins the no-target guard.
#[test]
fn test_enter_teammate_no_target() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Agent("plain".into()));
    assert!(!app.enter_teammate_view(), "no Subagent line to target");
    assert!(app.teammate_view.is_none());
}

/// A cursor whose content row falls past the last transcript line misses
/// every line in the walk and falls back to the most recent Subagent.
/// Pins the walk's terminal None + the fallback so a cursor below the
/// tail still drills into the latest delegation.
#[test]
fn test_cursor_past_tail_fallback() {
    use crate::records::TranscriptLine;
    let mut app = crate::composition::app();
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        folded_transcript: vec![TranscriptLine::Agent("reply".into())],
    });
    // A content row well past the one line in the transcript.
    app.selection.start(999, 999);
    assert!(
        app.enter_teammate_view(),
        "cursor past tail falls back to the last Subagent"
    );
    assert_eq!(app.teammate_view.as_ref().unwrap().child_sid, "c1");
}

/// A collapsed fold-group before a Subagent shifts the Subagent's content
/// row in the fold-aware space the mouse sets. The cursor walk mirrors the
/// render path (display_slots), so a click on the first of two delegations
/// after a folded tool group targets that delegation rather than the
/// fallback last.
#[test]
fn test_cursor_after_fold_group() {
    use crate::records::{ToolOutcome, TranscriptLine};
    let mut app = crate::composition::app();
    // Two tool calls form a collapsed Summary slot before the Subagents.
    app.transcript.push(TranscriptLine::Tool {
        name: "bash".into(),
        tool: "bash".into(),
        status: "ls".into(),
        invocation: "ls".into(),
        outcome: ToolOutcome::Success,
        call_id: "t1".into(),
        body: String::new(),
        is_diff: false,
    });
    app.transcript.push(TranscriptLine::Tool {
        name: "result".into(),
        tool: "bash".into(),
        status: String::new(),
        invocation: String::new(),
        outcome: ToolOutcome::Success,
        call_id: "t1".into(),
        body: "ok\nline2\nline3".into(),
        is_diff: false,
    });
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c1".into(),
        subagent_type: "explore".into(),
        summary: "first".into(),
        folded_transcript: vec![TranscriptLine::Agent("reply1".into())],
    });
    app.transcript.push(TranscriptLine::Subagent {
        child_sid: "c2".into(),
        subagent_type: "plan".into(),
        summary: "second".into(),
        folded_transcript: vec![TranscriptLine::Agent("reply2".into())],
    });
    // c1 sits at transcript index 2. fold_aware_rows gives its start row in
    // the same space the mouse sets content_row. A flat walk would land it
    // one slot earlier inside the Summary and miss, falling back to c2.
    let c1_row = app.fold_aware_rows(Some(2));
    app.selection.start(0, c1_row);
    assert!(app.enter_teammate_view(), "cursor on c1 enters");
    assert_eq!(
        app.teammate_view.as_ref().unwrap().child_sid,
        "c1",
        "cursor after a folded group targets the clicked delegation, not the fallback"
    );
    // The flat walk would have resolved to c2 (last) — confirm it does not.
    assert!(
        !app.expanded_subagents.contains("c2"),
        "the non-targeted delegation is untouched"
    );
}
