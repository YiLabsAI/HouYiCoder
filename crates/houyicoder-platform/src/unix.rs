//! Shared unix primitives: the terminal boundary a session runs against,
//! the shell discovery that reports a POSIX host keeps its own shell, and
//! the clipboard delivery that owns the tmux buffer and the bounded
//! subprocess wait.

pub mod clipboard;
pub mod shell;
pub(crate) mod terminal;
