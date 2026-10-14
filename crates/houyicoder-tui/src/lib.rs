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
mod fleet;
pub mod fold;
pub mod git_op;
pub mod history;
mod hooks_state;
pub mod input;
pub mod keys;
mod layout_feedback;
mod layout_snapshot;
mod list_pane_state;
pub mod markdown;
mod memory_state;
mod model_receipt;
pub mod notifications;
pub mod palette;
pub mod paste;
pub mod pending_prompt;
mod permission_input;
mod permission_pane_state;
pub mod records;
pub mod redaction;
pub mod render_cache;
mod renderer;
pub mod result_body;
pub mod resume_picker;
pub mod review_queue;
pub mod row_id;
mod rows;
pub mod run_control;
mod run_metrics;
pub mod run_state;
pub mod scroll;
pub mod selection;
mod selections;
pub mod session;
mod sessions;
mod skill_picker_state;
mod skills_state;
pub mod state;
mod terminal_guard;
pub mod terminal_title;
pub mod todo_view;
mod toggle_hint;
pub mod transcript;
pub mod ui;
pub mod view;
mod workflow;

#[cfg(feature = "bench")]
pub mod bench_api;

#[cfg(test)]
mod activity_indicator_tests;
#[cfg(test)]
mod artifact_tests;
#[cfg(test)]
mod frame_timing;
#[cfg(test)]
mod history_read_tests;
#[cfg(test)]
mod jump_to_bottom_tests;
#[cfg(test)]
mod permission_render_tests;
#[cfg(test)]
mod queue_sync_tests;
#[cfg(test)]
mod render_invariant_tests;
#[path = "resume_picker_tests.rs"]
mod resume_picker_tests;
#[cfg(test)]
mod test_harness;
#[cfg(test)]
mod transcript_rebuild_tests;
