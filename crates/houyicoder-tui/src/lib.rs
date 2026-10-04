//! Terminal frontend for session interaction.
//!
//! Renders the transcript, input, approvals, status, and on-demand panes.
//! Requests and events cross the wire protocol through the client; runtime
//! engine types and shared engine state do not enter this crate.

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
pub mod history;
pub mod input;
pub mod keys;
mod list_pane_state;
pub mod markdown;
mod memory_state;
mod model_receipt;
pub mod notifications;
pub mod palette;
pub mod paste;
pub mod pending_prompt;
pub mod pending_queue;
mod permission_input;
pub mod records;
pub mod redaction;
pub mod render_cache;
pub mod result_body;
pub mod resume_picker;
pub mod review_queue;
pub mod run_control;
pub mod run_state;
pub mod scroll;
pub mod selection;
pub mod session;
mod skills_state;
pub mod state;
pub mod terminal_title;
pub mod todo_view;
mod toggle_hint;
pub mod transcript;
pub mod view;

#[cfg(feature = "bench")]
pub mod bench_api;

#[cfg(test)]
mod activity_indicator_tests;
#[cfg(test)]
mod artifact_tests;
#[cfg(test)]
mod ask_question_render_tests;
#[cfg(test)]
mod ask_question_tests;
#[cfg(test)]
mod development_cycle_tests;
#[cfg(test)]
mod export_command_tests;
#[cfg(test)]
mod frame_timing;
#[cfg(test)]
mod history_read_tests;
#[cfg(test)]
mod jump_to_bottom_tests;
#[cfg(test)]
mod memory_pane_tests;
#[cfg(test)]
mod paste_submit_tests;
#[cfg(test)]
mod permission_render_tests;
#[cfg(test)]
mod permission_tests;
#[cfg(test)]
mod queue_sync_tests;
#[cfg(test)]
mod queue_view_tests;
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
mod slash_command_tests;
#[cfg(test)]
mod snapshot_render_tests;
#[cfg(test)]
mod spec_transition_tests;
#[cfg(test)]
mod teammate_scroll_tests;
#[cfg(test)]
mod teammate_view_tests;
#[cfg(test)]
mod test_harness;
#[cfg(test)]
mod todolist_verify_tests;
#[cfg(test)]
#[path = "transcript_rebuild_tests.rs"]
mod transcript_rebuild_tests;
#[cfg(test)]
mod viewport_tests;
