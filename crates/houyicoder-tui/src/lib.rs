//! Terminal frontend for session interaction.
//!
//! Renders the transcript, input, approvals, status, and on-demand panes.
//! Requests and events cross the wire protocol through the client; runtime
//! engine types and shared engine state do not enter this crate.

pub mod app;
pub mod command;
pub mod composition;
pub mod input;
pub mod keys;
pub mod session;
pub mod transcript;
pub mod ui;
pub mod view;
pub(crate) mod workflow;

#[cfg(feature = "bench")]
pub mod bench_api;

#[cfg(test)]
mod frame_timing;
#[cfg(test)]
mod test_harness;

pub use app::state;
pub(crate) use composition::terminal_guard;
pub use composition::terminal_title;
pub use input::{history, palette, paste};
pub use session::{
    agent_message, approval, ask_question_model, pending_prompt, run_control, run_state, todo_view,
};
pub(crate) use session::{model_receipt, run_metrics, sessions};
pub use transcript::{
    brief, fold, git_op, markdown, records, redaction, result_body, row_id, scroll,
};
pub(crate) use ui::{layout_feedback, layout_snapshot, renderer, rows, selections, toggle_hint};
pub use ui::{notifications, render_cache, selection};
pub use view::console::state as console_state;
pub use view::resume_picker::state as resume_picker;
pub(crate) use view::{
    capability::input as permission_input, capability::state as permission_pane_state, fleet,
    hooks_pane::state as hooks_state, list_pane_state, memory_pane::state as memory_state,
    skill_picker::state as skill_picker_state, skills_pane::state as skills_state,
};
pub use workflow::{artifact, evidence, review_queue};
