use super::*;
use crate::skills_state::SkillDetail;

#[test]
fn test_agents_result_stores_directory() {
    let mut app = crate::composition::app();
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(9),
        response: ServerResponse::Agents {
            directory: "## Available agents\n\n- explore: fast".into(),
        },
    });
    assert_eq!(
        app.agent_directory.as_deref(),
        Some("## Available agents\n\n- explore: fast"),
    );
}

#[test]
fn test_tool_list_result_stored() {
    let mut app = crate::composition::app();
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(10),
        response: ServerResponse::Tools {
            tools: vec![houyicoder_protocol::frontend::tools::ToolEntry {
                name: "bash".into(),
                description: "run a command".into(),
            }],
        },
    });
    assert_eq!(app.tool_entries.len(), 1);
    assert_eq!(app.tool_entries[0].name, "bash");
}

#[test]
fn test_skills_result_stored() {
    let mut app = crate::composition::app();
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(11),
        response: ServerResponse::Skills {
            skills: vec![houyicoder_protocol::frontend::skills::SkillEntry {
                name: "pdf-export".into(),
                description: "export chat to pdf".into(),
                origin: "user".into(),
                invocable: true,
                body_token_estimate: 320,
                user_invocable: true,
                usage: None,
            }],
        },
    });
    assert_eq!(app.skill_entries.len(), 1);
    assert_eq!(app.skill_entries[0].name, "pdf-export");
    assert_eq!(app.skill_entries[0].origin, "user");
    assert!(app.skill_entries[0].invocable);
}

/// A body reply for the request the detail waits on fills it with the text
/// the model would read on invocation.
#[test]
fn test_skill_body_fills_detail() {
    let mut app = crate::composition::app();
    app.skills_pane
        .request_detail(RequestId(21), "alpha".into());
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(21),
        response: ServerResponse::SkillBody {
            body: Some("run the alpha step".into()),
        },
    });
    assert_eq!(app.skills_pane.detail_body(), Some("run the alpha step"));
}

/// A reply for a superseded request leaves the current detail loading: the
/// body it carries belongs to a detail the user already replaced.
#[test]
fn test_stale_skill_body_dropped() {
    let mut app = crate::composition::app();
    app.skills_pane.request_detail(RequestId(22), "beta".into());
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(21),
        response: ServerResponse::SkillBody {
            body: Some("stale".into()),
        },
    });
    assert!(matches!(
        app.skills_pane.detail(),
        Some(SkillDetail::Loading { .. })
    ));
    assert!(app.skills_pane.detail_body().is_none());
}

/// A session-less App cannot request a body, so Enter opens the detail with
/// the body unavailable rather than leaving it loading forever.
#[test]
fn test_open_detail_without_session() {
    let mut app = crate::composition::app();
    app.pane = Pane::Skills;
    app.skill_entries = vec![skill_entry("alpha")];
    app.open_skill_detail();
    assert!(matches!(
        app.skills_pane.detail(),
        Some(SkillDetail::Open { .. })
    ));
    assert!(app.skills_pane.detail_body().is_none());
}

/// A send the lost connection refuses settles the detail it opened, so the
/// pane never waits on a reply that cannot arrive.
#[test]
fn test_lost_send_settles_detail() {
    let mut app = crate::test_harness::connection_lost_app();
    app.pane = Pane::Skills;
    app.skill_entries = vec![skill_entry("alpha")];
    app.open_skill_detail();
    assert!(matches!(
        app.skills_pane.detail(),
        Some(SkillDetail::Open { .. })
    ));
    assert!(app.skills_pane.detail_body().is_none());
}

/// A connection loss settles a loading detail, because no reply is coming.
#[test]
fn test_loss_settles_skill_detail() {
    let (mut app, _events) = crate::test_harness::connected_app_with_events();
    app.skills_pane
        .request_detail(RequestId(31), "alpha".into());
    assert!(app.apply_connection_loss("connect failed: no server".into(), Vec::new()));
    assert!(matches!(
        app.skills_pane.detail(),
        Some(SkillDetail::Open { .. })
    ));
    assert!(app.skills_pane.detail_body().is_none());
}

/// An error reply for the request the detail waits on settles it as
/// unavailable, the same way a connection loss does, so the pane never waits
/// on a reply that cannot arrive.
#[test]
fn test_error_settles_skill_detail() {
    let (mut app, _events) = crate::test_harness::connected_app_with_events();
    app.skills_pane
        .request_detail(RequestId(41), "alpha".into());
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(41),
        response: ServerResponse::Error {
            message: "body read failed".into(),
        },
    });
    assert!(matches!(
        app.skills_pane.detail(),
        Some(SkillDetail::Open { .. })
    ));
    assert!(app.skills_pane.detail_body().is_none());
    use crate::records::TranscriptLine;
    assert!(
        app.transcript.iter().any(
            |l| matches!(l, TranscriptLine::System(s) if s.contains("error: body read failed"))
        ),
        "the error still reaches the transcript"
    );
}

/// An error reply for a different request leaves a loading detail alone: the
/// reply it waits on is still to come.
#[test]
fn test_error_keeps_detail_loading() {
    let (mut app, _events) = crate::test_harness::connected_app_with_events();
    app.skills_pane
        .request_detail(RequestId(42), "alpha".into());
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(41),
        response: ServerResponse::Error {
            message: "body read failed".into(),
        },
    });
    assert!(matches!(
        app.skills_pane.detail(),
        Some(SkillDetail::Loading { .. })
    ));
}

/// t flips the session disable for the invocable skill the cursor points at.
#[test]
fn test_toggle_skill_at_cursor() {
    let mut app = crate::composition::app();
    app.pane = Pane::Skills;
    app.skill_entries = vec![skill_entry("alpha")];
    app.toggle_skill_at_cursor();
    assert!(
        app.skill_disabled.contains("alpha"),
        "the first press disables"
    );
    app.toggle_skill_at_cursor();
    assert!(
        !app.skill_disabled.contains("alpha"),
        "the second press re-enables"
    );
}

/// A skill the frontmatter blocks has no session state to flip, so t leaves
/// it blocked until its file changes.
#[test]
fn test_toggle_blocked_skill_noop() {
    let mut app = crate::composition::app();
    app.pane = Pane::Skills;
    let mut blocked = skill_entry("alpha");
    blocked.user_invocable = false;
    blocked.invocable = false;
    app.skill_entries = vec![blocked];
    app.toggle_skill_at_cursor();
    assert!(
        app.skill_disabled.is_empty(),
        "a blocked skill stays blocked"
    );
}

/// The /skills pane renders the discovered list with name, description,
/// source tag, and body token estimate per row. A populated list never
/// shows the empty placeholder.
#[test]
fn test_skills_pane_renders_entries() {
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
    let out = crate::test_harness::render_text(&app, 80, 24);
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
    let out = crate::test_harness::render_text(&app, 80, 24);
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
    let out = crate::test_harness::render_text(&app, 80, 24);
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
    let out = crate::test_harness::render_text(&app, 80, 24);
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
    let out = crate::test_harness::render_text(&app, 80, 24);
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
    let out = crate::test_harness::render_text(&app, 80, 24);
    assert!(
        out.contains("(no agent directory loaded)"),
        "empty directory shows placeholder, not blank: {out}"
    );
}
