//! The southbound ACP client. Launches an external ACP-compatible process,
//! drives its stdin/stdout pipe, and folds its session updates into a child
//! session log. The parent runtime uses this to delegate work to an external
//! agent (a different model, a review tool) under the same fence and
//! permission gate that governs in-process children.

pub mod permission;
pub mod session;
mod transcript;
