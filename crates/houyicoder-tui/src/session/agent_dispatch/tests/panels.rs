use super::*;
use crate::composition::app;
use crate::records::TranscriptLine;
use crate::skills_state::SkillDetail;
use crate::state::Pane;
use crate::state::Screen;
use crate::test_harness::agent_entry;
use crate::test_harness::connected_app_with_events;
use crate::test_harness::connection_lost_app;
use crate::test_harness::install_agent_types;
use crate::test_harness::render_text;
use crate::test_harness::worktree_path;
use crate::test_harness::worktree_preview;
use crate::ui::WorktreeLevel;
use houyicoder_protocol::frontend::skills::SkillEntry;
use houyicoder_protocol::frontend::tools::ToolEntry;

#[test]
fn test_agents_result_stores_types() {
    let mut app = app();
    let entry = agent_entry("explore", "fast");
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(9),
        response: ServerResponse::Agents {
            entries: vec![entry.clone()],
        },
    });
    assert_eq!(
        app.sessions.current.capabilities.agent_types.entries,
        Some(vec![entry]),
    );
}

#[test]
fn test_tool_list_result_stored() {
    let mut app = app();
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(10),
        response: ServerResponse::Tools {
            tools: vec![ToolEntry {
                name: "bash".into(),
                description: "run a command".into(),
            }],
        },
    });
    assert_eq!(app.sessions.current.capabilities.tools.entries.len(), 1);
    assert_eq!(
        app.sessions.current.capabilities.tools.entries[0].name,
        "bash"
    );
}

#[test]
fn test_skills_result_stored() {
    let mut app = app();
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(11),
        response: ServerResponse::Skills {
            skills: vec![SkillEntry {
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
    assert_eq!(app.sessions.current.capabilities.skills.entries.len(), 1);
    assert_eq!(
        app.sessions.current.capabilities.skills.entries[0].name,
        "pdf-export"
    );
    assert_eq!(
        app.sessions.current.capabilities.skills.entries[0].origin,
        "user"
    );
    assert!(app.sessions.current.capabilities.skills.entries[0].invocable);
}

/// A body reply for the request the detail waits on fills it with the text
/// the model would read on invocation.
#[test]
fn test_skill_body_fills_detail() {
    let mut app = app();
    app.ui
        .skills_pane
        .request_detail(RequestId(21), "alpha".into());
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(21),
        response: ServerResponse::SkillBody {
            body: Some("run the alpha step".into()),
        },
    });
    assert_eq!(app.ui.skills_pane.detail_body(), Some("run the alpha step"));
}

/// A reply for a superseded request leaves the current detail loading: the
/// body it carries belongs to a detail the user already replaced.
#[test]
fn test_stale_skill_body_dropped() {
    let mut app = app();
    app.ui
        .skills_pane
        .request_detail(RequestId(22), "beta".into());
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(21),
        response: ServerResponse::SkillBody {
            body: Some("stale".into()),
        },
    });
    assert!(matches!(
        app.ui.skills_pane.detail(),
        Some(SkillDetail::Loading { .. })
    ));
    assert!(app.ui.skills_pane.detail_body().is_none());
}

/// A session-less App cannot request a body, so Enter opens the detail with
/// the body unavailable rather than leaving it loading forever.
#[test]
fn test_open_detail_without_session() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Skills;
    app.sessions.current.capabilities.skills.entries = vec![skill_entry("alpha")];
    app.open_skill_detail();
    assert!(matches!(
        app.ui.skills_pane.detail(),
        Some(SkillDetail::Open { .. })
    ));
    assert!(app.ui.skills_pane.detail_body().is_none());
}

/// A send the lost connection refuses settles the detail it opened, so the
/// pane never waits on a reply that cannot arrive.
#[test]
fn test_lost_send_settles_detail() {
    let mut app = connection_lost_app();
    app.ui.navigation.pane = Pane::Skills;
    app.sessions.current.capabilities.skills.entries = vec![skill_entry("alpha")];
    app.open_skill_detail();
    assert!(matches!(
        app.ui.skills_pane.detail(),
        Some(SkillDetail::Open { .. })
    ));
    assert!(app.ui.skills_pane.detail_body().is_none());
}

/// A connection loss settles a loading detail, because no reply is coming.
#[test]
fn test_loss_settles_skill_detail() {
    let (mut app, _events) = connected_app_with_events();
    app.ui
        .skills_pane
        .request_detail(RequestId(31), "alpha".into());
    assert!(app.apply_connection_loss("connect failed: no server".into(), Vec::new()));
    assert!(matches!(
        app.ui.skills_pane.detail(),
        Some(SkillDetail::Open { .. })
    ));
    assert!(app.ui.skills_pane.detail_body().is_none());
}

/// An error reply for the request the detail waits on settles it as
/// unavailable, the same way a connection loss does, so the pane never waits
/// on a reply that cannot arrive.
#[test]
fn test_error_settles_skill_detail() {
    let (mut app, _events) = connected_app_with_events();
    app.ui
        .skills_pane
        .request_detail(RequestId(41), "alpha".into());
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(41),
        response: ServerResponse::Error {
            message: "body read failed".into(),
        },
    });
    assert!(matches!(
        app.ui.skills_pane.detail(),
        Some(SkillDetail::Open { .. })
    ));
    assert!(app.ui.skills_pane.detail_body().is_none());
    assert!(
        app.sessions.current.transcript.iter().any(
            |l| matches!(l, TranscriptLine::System(s) if s.contains("error: body read failed"))
        ),
        "the error still reaches the transcript"
    );
}

/// An error reply for a different request leaves a loading detail alone: the
/// reply it waits on is still to come.
#[test]
fn test_error_keeps_detail_loading() {
    let (mut app, _events) = connected_app_with_events();
    app.ui
        .skills_pane
        .request_detail(RequestId(42), "alpha".into());
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(41),
        response: ServerResponse::Error {
            message: "body read failed".into(),
        },
    });
    assert!(matches!(
        app.ui.skills_pane.detail(),
        Some(SkillDetail::Loading { .. })
    ));
}

/// t flips the session disable for the invocable skill the cursor points at.
#[test]
fn test_toggle_skill_at_cursor() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Skills;
    app.sessions.current.capabilities.skills.entries = vec![skill_entry("alpha")];
    app.toggle_skill_at_cursor();
    assert!(
        app.sessions
            .current
            .capabilities
            .skills
            .disabled
            .contains("alpha"),
        "the first press disables"
    );
    app.toggle_skill_at_cursor();
    assert!(
        !app.sessions
            .current
            .capabilities
            .skills
            .disabled
            .contains("alpha"),
        "the second press re-enables"
    );
}

/// A skill the frontmatter blocks has no session state to flip, so t leaves
/// it blocked until its file changes.
#[test]
fn test_toggle_blocked_skill_noop() {
    let mut app = app();
    app.ui.navigation.pane = Pane::Skills;
    let mut blocked = skill_entry("alpha");
    blocked.user_invocable = false;
    blocked.invocable = false;
    app.sessions.current.capabilities.skills.entries = vec![blocked];
    app.toggle_skill_at_cursor();
    assert!(
        app.sessions.current.capabilities.skills.disabled.is_empty(),
        "a blocked skill stays blocked"
    );
}

/// The /skills pane renders the discovered list with name, description,
/// source tag, and body token estimate per row. A populated list never
/// shows the empty placeholder.
#[test]
fn test_skills_pane_renders_entries() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Skills;
    app.sessions.current.capabilities.skills.entries = vec![
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
    let out = render_text(&app, 80, 24);
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
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Skills;
    app.sessions.current.capabilities.skills.entries = vec![
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
    let out = render_text(&app, 80, 24);
    println!("--- /skills pane showcase (80x24) ---\n{out}\n--- end ---");
    assert!(out.contains("Skills"), "pane title renders");
    assert!(out.contains("3 skills discovered"), "count line renders");
    assert!(out.contains("Esc to close"), "close hint renders");
}

#[test]
fn test_tools_pane_renders_entries() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Tools;
    app.sessions.current.capabilities.tools.entries = vec![
        ToolEntry {
            name: "zed".into(),
            description: "edits files".into(),
        },
        ToolEntry {
            name: "bash".into(),
            description: "runs a command\nsecond line".into(),
        },
    ];
    let out = render_text(&app, 80, 24);
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
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Tools;
    app.sessions.current.capabilities.tools.entries = vec![];
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("(no tools loaded)"),
        "empty tools renders placeholder: {out}"
    );
}

#[test]
fn test_agents_types_render_rows() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Agents;
    install_agent_types(
        &mut app,
        &[("explore", "fast search"), ("plan", "design a step")],
    );
    let out = render_text(&app, 80, 24);
    let heading_row = out.lines().position(|l| l.contains("Available types"));
    let explore_row = out.lines().position(|l| l.contains("explore"));
    assert!(heading_row.is_some(), "heading renders: {out}");
    assert!(explore_row.is_some(), "explore renders: {out}");
    assert!(
        heading_row.unwrap() < explore_row.unwrap(),
        "the heading must sit above the type row (per-line render), not flatten to one row"
    );
}

/// A reply carrying no types renders the placeholder, not a blank pane, so the
/// fallback fires on an empty list.
#[test]
fn test_agents_types_empty_placeholder() {
    let mut app = app();
    app.ui.navigation.screen = Screen::Working;
    app.ui.navigation.pane = Pane::Agents;
    install_agent_types(&mut app, &[]);
    let out = render_text(&app, 80, 24);
    assert!(
        out.contains("(no agent types registered)"),
        "an empty type list shows the placeholder, not a blank pane: {out}"
    );
}

/// The writable-directory list arrives whole, so the transcript line that
/// names a directory has to come from the difference against the list the
/// pane already held. A line written when the request went out names nothing
/// and claims an effect the host may refuse; this asserts the reported line
/// carries the path and the direction.
#[test]
fn test_permission_dirs_retained() {
    let mut app = app();
    app.sessions.current.capabilities.permission.dirs_cache = vec!["/old".into()];
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(11),
        response: ServerResponse::PermissionDirs {
            dirs: vec!["/old".into(), "/tmp/extra".into()],
        },
    });
    let last = app
        .sessions
        .current
        .transcript
        .iter()
        .rev()
        .find_map(|l| match l {
            TranscriptLine::System(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(last, "permission: added writable directory /tmp/extra");
    assert_eq!(
        app.sessions.current.capabilities.permission.dirs_cache,
        vec!["/old".to_string(), "/tmp/extra".to_string()],
        "the cache takes the host's list"
    );
}

/// The removal direction, so the two sentences cannot be swapped: a directory
/// the host dropped is named, and the one it kept is not.
#[test]
fn test_permission_dirs_dropped() {
    let mut app = app();
    app.sessions.current.capabilities.permission.dirs_cache =
        vec!["/old".into(), "/tmp/extra".into()];
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(12),
        response: ServerResponse::PermissionDirs {
            dirs: vec!["/old".into()],
        },
    });
    let last = app
        .sessions
        .current
        .transcript
        .iter()
        .rev()
        .find_map(|l| match l {
            TranscriptLine::System(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default();
    assert_eq!(last, "permission: removed writable directory /tmp/extra");
}

/// An unchanged list reports nothing: a refresh must not replay the whole
/// grant set into the transcript as if every directory had just been added.
#[test]
fn test_permission_dirs_unchanged() {
    let mut app = app();
    let before = app.sessions.current.transcript.len();
    app.sessions.current.capabilities.permission.dirs_cache = vec!["/old".into()];
    app.handle_agent_message(SessionMessage::Response {
        request: RequestId(13),
        response: ServerResponse::PermissionDirs {
            dirs: vec!["/old".into()],
        },
    });
    assert_eq!(
        app.sessions.current.transcript.len(),
        before,
        "an unchanged list adds no line"
    );
}

/// A confirmation page waiting on a request the dead transport can never
/// answer must let go of it. While the request is out the page ignores every
/// key and offers no button, so a request that is never settled leaves the
/// user with no way off the page at all.
#[test]
fn test_loss_settles_worktree_removal() {
    let (mut app, _events) = connected_app_with_events();
    let path = worktree_path("alpha");
    {
        let mut draft = app.ui.worktree_pane.draft.borrow_mut();
        draft.begin(&path, WorktreeLevel::List, None);
        draft.accept(worktree_preview("alpha"));
        draft
            .take_submission(RequestId(41))
            .expect("a free worktree submits");
    }
    app.ui.worktree_pane.level.set(WorktreeLevel::Confirm);
    assert!(app.ui.worktree_pane.draft.borrow().removing());

    assert!(app.apply_connection_loss("connect failed".into(), Vec::new()));

    assert!(
        !app.ui.worktree_pane.draft.borrow().removing(),
        "the removal stops waiting on an answer that cannot arrive"
    );
}

/// The same release for the sandbox restore confirmation.
#[test]
fn test_loss_settles_restore() {
    let (mut app, _events) = connected_app_with_events();
    assert!(app.ui.sandbox_pane.begin_restore(RequestId(43)));

    assert!(app.apply_connection_loss("connect failed".into(), Vec::new()));

    assert!(
        app.ui.sandbox_pane.restore_request().is_none(),
        "the restore stops waiting on an answer that cannot arrive"
    );
}
