//! Console-screen state: the slim work-inbox todo column (PR / MR / issue /
//! CI stubs). Extracted from App. The review queue (findings + audit) lives
//! in ReviewQueue; this holds only the todo inbox column.

use crate::evidence::ConsoleTodo;

/// Console dashboard state: the work-inbox todo column. The review queue is
/// separate; this is the PR / MR / issue / CI stub list.
#[derive(Debug, Clone, Default)]
pub struct ConsoleState {
    /// Work-inbox todos (PR / MR / issue / CI stubs).
    pub todos: Vec<ConsoleTodo>,
}

impl ConsoleState {
    /// Number of todos in the inbox.
    pub fn todo_len(&self) -> usize {
        self.todos.len()
    }
}
