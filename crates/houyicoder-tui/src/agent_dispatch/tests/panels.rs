use super::*;

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

#[test]
fn test_skills_result_stored() {
    let mut app = crate::composition::app();
    app.handle_agent_message(AgentMessage::SkillsResult {
        skills: vec![houyicoder_protocol::frontend::skills::SkillEntry {
            name: "pdf-export".into(),
            description: "export chat to pdf".into(),
            origin: "user".into(),
            invocable: true,
            body_token_estimate: 320,
            user_invocable: true,
            usage: None,
        }],
    });
    assert_eq!(app.skill_entries.len(), 1);
    assert_eq!(app.skill_entries[0].name, "pdf-export");
    assert_eq!(app.skill_entries[0].origin, "user");
    assert!(app.skill_entries[0].invocable);
}

/// The /skills pane renders the discovered list with name, description,
/// source tag, and body token estimate per row. A populated list never
/// shows the empty placeholder.
#[test]
fn test_skills_pane_renders_entries() {
    use houyicoder_protocol::frontend::skills::SkillEntry;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.pane = crate::state::Pane::Skills;
    app.skill_entries = vec![
        SkillEntry {
            name: "pdf-export".into(),
            description: "export chat to pdf".into(),
            origin: "user".into(),
            invocable: true,
            body_token_estimate: 320,
            user_invocable: true,
            usage: None,
        },
        SkillEntry {
            name: "internal-only".into(),
            description: "host-restricted".into(),
            origin: "project".into(),
            invocable: false,
            body_token_estimate: 80,
            user_invocable: true,
            usage: None,
        },
    ];
    let out = crate::test_support::render_text(&app, 80, 24);
    assert!(out.contains("pdf-export"), "name row renders: {out}");
    assert!(out.contains("internal-only"), "second name renders: {out}");
    assert!(
        out.matches("Native —").count() >= 2,
        "both native groups (user + project) render: {out}"
    );
    assert!(out.contains("320"), "token estimate renders: {out}");
    assert!(
        !out.contains("No skills found"),
        "populated list hides the empty placeholder: {out}"
    );
}

/// UX showcase: the /skills pane with a realistic mixed list — a
/// model-invocable skill, a disabled one, and a large one — so the
/// render format (name, description, source tag, token cost) can be
/// eyeballed. Run with --nocapture to view.
#[test]
fn test_skills_pane_showcase() {
    use houyicoder_protocol::frontend::skills::SkillEntry;
    let mut app = crate::composition::app();
    app.screen = crate::state::Screen::Working;
    app.pane = crate::state::Pane::Skills;
    app.skill_entries = vec![
        SkillEntry {
            name: "pdf-export".into(),
            description: "export the chat transcript to a pdf file".into(),
            origin: "user".into(),
            invocable: true,
            body_token_estimate: 1_240,
            user_invocable: true,
            usage: None,
        },
        SkillEntry {
            name: "commit".into(),
            description: "stage and commit with a conventional message".into(),
            origin: "user".into(),
            invocable: true,
            body_token_estimate: 320,
            user_invocable: true,
            usage: None,
        },
        SkillEntry {
            name: "internal-only".into(),
            description: "host-restricted, not callable by the model".into(),
            origin: "project".into(),
            invocable: false,
            body_token_estimate: 80,
            user_invocable: true,
            usage: None,
        },
    ];
    let out = crate::test_support::render_text(&app, 80, 24);
    println!("--- /skills pane showcase (80x24) ---\n{out}\n--- end ---");
    assert!(out.contains("Skills"), "pane title renders");
    assert!(out.contains("3 skills discovered"), "count line renders");
    assert!(out.contains("Esc to close"), "close hint renders");
}

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
