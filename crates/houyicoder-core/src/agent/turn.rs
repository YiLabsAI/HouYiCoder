//! Single-turn execution, tool-call dispatch, streaming fold, and recovery.

pub(crate) mod abort;
pub(crate) mod approval;
pub(crate) mod call;
pub(crate) mod fallback;
pub(crate) mod input_queue;
pub(crate) mod outcome_counts;
pub(crate) mod recover;
pub(crate) mod resolve;
pub(crate) mod result;
pub(crate) mod run_lifecycle;
pub(crate) mod step;
pub(crate) mod stream_fold;
pub(crate) mod thinking;
pub(crate) mod truncation;
pub(crate) mod user_turn;
pub(crate) mod verify;
