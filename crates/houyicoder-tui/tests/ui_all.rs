//! Consolidated PTY and contract integration tests. Each module is the
//! original test file included verbatim via path attributes; aggregating
//! into one binary cuts nextest discovery spawn count without dropping any
//! test or coverage. live_agent stays separate because nextest filters it
//! by name and it pulls the real network provider.

mod common;

#[path = "build_runner.rs"]
mod build_runner;
#[path = "ui_approval.rs"]
mod ui_approval;
#[path = "ui_consent.rs"]
mod ui_consent;
#[path = "ui_context.rs"]
mod ui_context;
#[path = "ui_diff.rs"]
mod ui_diff;
#[path = "ui_entitlement.rs"]
mod ui_entitlement;
#[path = "ui_exit_keys.rs"]
mod ui_exit_keys;
#[path = "ui_fence.rs"]
mod ui_fence;
#[path = "ui_hooks.rs"]
mod ui_hooks;
#[path = "ui_input.rs"]
mod ui_input;
#[path = "ui_memory.rs"]
mod ui_memory;
#[path = "ui_mode.rs"]
mod ui_mode;
#[path = "ui_model.rs"]
mod ui_model;
#[path = "ui_multiagent.rs"]
mod ui_multiagent;
#[path = "ui_palette.rs"]
mod ui_palette;
#[path = "ui_permissions.rs"]
mod ui_permissions;
#[path = "ui_resume.rs"]
mod ui_resume;
#[path = "ui_run.rs"]
mod ui_run;
#[path = "ui_search.rs"]
mod ui_search;
#[path = "ui_session.rs"]
mod ui_session;
#[path = "ui_skill_ecosystem.rs"]
mod ui_skill_ecosystem;
#[path = "ui_skill_hotreload.rs"]
mod ui_skill_hotreload;
#[path = "ui_skill_paths.rs"]
mod ui_skill_paths;
#[path = "ui_thinking.rs"]
mod ui_thinking;
#[path = "ui_todo.rs"]
mod ui_todo;
#[path = "ui_tools.rs"]
mod ui_tools;
#[path = "ui_worktree.rs"]
mod ui_worktree;
#[path = "ux_commands.rs"]
mod ux_commands;
