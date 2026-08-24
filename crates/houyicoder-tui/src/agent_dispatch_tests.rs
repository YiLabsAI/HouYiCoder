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
        out.contains("child transcript loads on fetch"),
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
