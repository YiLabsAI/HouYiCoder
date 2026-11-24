//! Concrete tools backed by a SandboxSession. BashTool runs shell;
//! Read/Write/Edit/MultiEdit operate on session files. Destructive tools
//! declare requires_approval so the loop gates them behind a human
//! decision.
//!
//! Submodules are private; the public surface is the named re-exports
//! below. Internal callers within the tools tree reach siblings via
//! crate::agent::, and cross-crate consumers go through the agent facade.

pub(crate) mod diff;
pub(crate) mod registry;

use houyicoder_api::tool::{Tool, ToolCtx};
use houyicoder_protocol::extension::ToolError;

mod ask_user_question;
mod bash_snapshot;
mod bash_tool;
mod conversation_search;
mod delegation;
mod edit;
mod file_edit;
mod glob;
mod grep;
mod memory_archive;
mod memory_delete;
mod memory_promote_demote;
mod memory_read;
mod memory_search;
mod memory_write;
mod multiedit;
mod path_util;
mod read;
mod result_capture;
mod results;
mod skill;
mod subprocess_util;
mod task_tools;
mod todo;
mod webfetch;
mod worktree_enter;
mod worktree_exit;
mod write;

pub use ask_user_question::AskUserQuestionTool;
pub use bash_tool::BashTool;
pub use conversation_search::ConversationSearchTool;
pub use delegation::DelegationTool;
pub use delegation::bound_child_summary;
pub(crate) use delegation::orphaned_result;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use memory_archive::MemoryArchiveTool;
pub use memory_delete::MemoryDeleteTool;
pub use memory_promote_demote::{MemoryDemoteTool, MemoryPromoteTool};
pub use memory_read::MemoryReadTool;
pub use memory_search::MemorySearchTool;
pub use memory_write::MemoryWriteTool;
pub use multiedit::MultiEditTool;
pub use read::ReadTool;
pub use registry::ToolSource;
pub use results::SessionResultCatalog;
pub use skill::SkillTool;
pub use task_tools::{SendMessageTool, TaskListTool, TaskOutputTool, TaskStopTool};
pub use todo::{TodoItem, TodoStatus, TodoWriteTool};
pub use webfetch::WebFetchTool;
pub use worktree_enter::EnterWorktreeTool;
pub use worktree_exit::ExitWorktreeTool;
pub use write::WriteTool;

pub(crate) use result_capture::SessionResultCapture;

#[cfg(test)]
mod tests;
