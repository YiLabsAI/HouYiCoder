//! Concrete tools backed by a SandboxSession. BashTool runs shell;
//! Read/Write/Edit/MultiEdit operate on session files. Destructive tools
//! declare requires_approval so the loop gates them behind a human
//! decision.
//!
//! Submodules are private; the public surface is the named re-exports
//! below. Internal callers within the tools tree reach siblings via
//! super::, and cross-crate consumers go through the agent facade.

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
mod memory_add;
mod memory_delete;
mod memory_promote_demote;
mod memory_show;
mod multiedit;
mod path_util;
mod read;
mod skill;
mod subprocess_util;
mod todo;
mod webfetch;
mod worktree_enter;
mod worktree_exit;
mod write;

pub use ask_user_question::AskUserQuestionTool;
pub use bash_tool::BashTool;
pub use conversation_search::ConversationSearchTool;
pub use delegation::DelegationTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use memory_add::MemoryAddTool;
pub use memory_delete::DeleteMemoryTool;
pub use memory_promote_demote::{DemoteMemoryTool, PromoteMemoryTool};
pub use memory_show::ShowMemoryTool;
pub use multiedit::MultiEditTool;
pub use read::ReadTool;
pub use skill::SkillTool;
pub use todo::{TodoItem, TodoStatus, TodoWriteTool};
pub use webfetch::WebFetchTool;
pub use worktree_enter::EnterWorktreeTool;
pub use worktree_exit::ExitWorktreeTool;
pub use write::WriteTool;

#[cfg(test)]
#[path = "tools/file_tools_tests.rs"]
mod tests;
