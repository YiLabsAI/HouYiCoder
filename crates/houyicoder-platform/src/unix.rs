//! Shared unix primitives: the terminal boundary a session runs against,
//! and the shell discovery that reports a POSIX host keeps its own shell.

pub mod shell;
pub(crate) mod terminal;
