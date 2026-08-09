//! Hooks command tests split from scroll_tests.rs for file-size.

use super::working;

/// /hooks opens the Hooks pane (a live view, not a transcript system line) +
/// requests the hook list when a runner is wired.
#[test]
fn test_hooks_opens_pane_wired() {
    let mut app = crate::composition::build_app_for_test(None);
    app.run_command(houyicoder_protocol::frontend::SlashCommand::Hooks);
    assert_eq!(app.pane, crate::state::Pane::Hooks, "/hooks opens the pane");
}

/// /hooks with no runner wired still opens the pane (shows the empty hook
/// list row).
#[test]
fn test_opens_pane_no_runner() {
    let mut app = working();
    app.run_command(houyicoder_protocol::frontend::SlashCommand::Hooks);
    assert_eq!(app.pane, crate::state::Pane::Hooks);
    assert!(app.hook_entries.is_empty());
}

/// A HooksResult populates the pane's hook_entries cache (a live view).
#[test]
fn test_hooks_result_stores_entries() {
    use crate::run_control::AgentMessage;
    use houyicoder_protocol::frontend::hooks::HookEntry;
    let mut app = working();
    app.handle_agent_message(AgentMessage::HooksResult {
        hooks: vec![
            HookEntry {
                name: "pre-check".into(),
                events: vec!["PreToolUse".into()],
                source: "Project".into(),
                fired: true,
                summary: String::new(),
            },
            HookEntry {
                name: "PreToolUse".into(),
                events: vec!["PreToolUse".into()],
                source: "framework".into(),
                fired: true,
                summary: "Before tool execution".into(),
            },
        ],
    });
    assert_eq!(app.hook_entries.len(), 2);
    assert_eq!(app.hook_entries[0].name, "pre-check");
}
