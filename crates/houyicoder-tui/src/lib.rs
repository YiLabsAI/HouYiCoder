//! Terminal frontend for session interaction.
//!
//! Renders the transcript, input, approvals, status, and on-demand panes.
//! Requests and events cross the wire protocol through the client; runtime
//! engine types and shared engine state do not enter this crate.

pub mod app;
pub(crate) mod command;
pub mod composition;
pub mod input;
pub mod keys;
pub mod session;
pub mod transcript;
pub mod ui;
pub mod view;
pub mod workflow;

#[cfg(feature = "bench")]
pub mod bench_api;

#[cfg(test)]
mod frame_timing;
#[cfg(test)]
mod test_harness;

pub use app::state;
pub(crate) use composition::{terminal_guard, terminal_title};
pub(crate) use input::palette;
pub use input::{history, paste};
pub use session::{agent_message, pending_prompt};
pub(crate) use session::{
    ask_question_model, model_receipt, run_metrics, run_state, sessions, todo_view, turn_driver,
};
pub(crate) use transcript::{brief, fold, git_op, markdown, scroll};
pub use transcript::{records, redaction, result_body, row_id};
pub(crate) use ui::{
    layout_feedback, layout_snapshot, notifications, renderer, rows, selections, toggle_hint,
};
pub use ui::{render_cache, selection};
pub use view::console::state as console_state;
pub use view::resume_picker::state as resume_picker;
pub(crate) use view::{
    agents_pane::state as agents_state, capability::input as permission_input,
    capability::state as permission_pane_state, fleet, hooks_pane::state as hooks_state,
    list_pane_state, memory_pane::state as memory_state, skill_picker::state as skill_picker_state,
    skills_pane::state as skills_state,
};
pub use workflow::{artifact, evidence, review_queue};
