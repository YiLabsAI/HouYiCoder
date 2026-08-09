//! The terminal frontend for the engine.
//!
//! Renders the locked UX: a login screen, an enterprise console, and a coding
//! working surface (not a chat). The working surface is an activity log (action
//! log, scrollable and searchable) with a capability pane that switches by
//! state. A spec context strip with a three-stage progress bar (design /
//! implement / verify) stays visible at the top. Slash commands open a palette
//! over the protocol SlashCommand set.
//!
//! The guided chain is three stages: design (spec + plan, one approval),
//! implement (per-change diff approval), and verify (agent review + machine
//! check, one checkpoint). Typing a task + Enter auto-enters design. The
//! convergence loop lets review/verify rework back to implementing. One approval
//! pattern (approve/reject, shared components) and one color vocabulary govern
//! every screen. The status bar carries a contextual stage hint plus a token
//! budget bar that surfaces current token usage against the configured budget.
//!
//! The frontend speaks the wire protocol exclusively: a Client handle
//! (from the client crate) sends requests and receives events; no engine
//! type crosses into this crate at runtime. The TUI is a pure protocol
//! consumer — presentation over the wire, never over shared engine state.

#![allow(dead_code)] // crate root re-exports tui modules consumed by other crates; locally unused

pub mod agent_message;
pub mod app;
pub mod approval;
pub mod artifact;
pub mod ask_question_model;
pub mod brief;
pub mod command;
pub mod composition;
pub mod console_state;

pub mod evidence;
pub mod fold;
pub mod git_op;
pub mod input;
pub mod keys;
pub mod markdown;
pub mod palette;
pub mod paste;
pub mod pending_queue;
mod permission_input;
pub mod records;
pub mod redaction;
pub mod render_cache;
pub mod result_body;
pub mod resume_picker;
pub mod review_queue;
pub mod run_control;
pub mod scroll;
pub mod selection;
pub mod session;
pub mod state;
pub mod terminal_title;
pub mod todo_view;
pub mod transcript;
pub mod view;

#[cfg(test)]
mod artifact_tests;
#[cfg(test)]
mod ask_question_render_tests;
#[cfg(test)]
mod ask_question_tests;
#[cfg(test)]
mod drain_flow_tests;
#[cfg(test)]
mod export_command_tests;
#[cfg(test)]
mod flow_tests;
#[cfg(test)]
#[path = "interact_memory_tests.rs"]
mod interact_memory_tests;
#[cfg(test)]
mod interact_tests;
#[cfg(test)]
mod jump_pill_tests;
#[cfg(test)]
mod permission_render_tests;
#[cfg(test)]
mod permission_tests;
#[cfg(test)]
mod render_invariant_tests;
#[path = "resume_picker_tests.rs"]
mod resume_picker_tests;
#[cfg(test)]
#[path = "scroll_tests.rs"]
mod scroll_tests;
#[cfg(test)]
mod selection_clipboard_tests;
#[cfg(test)]
mod snapshot_render_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod todolist_verify_tests;
#[cfg(test)]
#[path = "transcript_seal_tests.rs"]
mod transcript_seal_tests;
#[cfg(test)]
mod viewport_tests;
