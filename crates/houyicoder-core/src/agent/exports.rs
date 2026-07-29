//! Public re-exports of the agent module API surface, split from mod.rs so
//! the module root stays under the file-size gate. The agent module path
//! still resolves these via mod.rs re-exporting exports.

pub use super::backbone::{
    BackboneDerivation, CompactBackbone, ConflictRate, GitWorkspaceProbe, StubWorkspaceProbe,
    WorkspaceProbe, derive_backbone, merge_summary, render_backbone_block,
};
pub use super::context::{
    CategoryBreakdown, ContextBreakdown, ContextBuilder, GridSquare, Section, SectionKind,
    ServedView, Tokenizer, build_grid, stub_breakdown,
};
pub use super::diff::unified_diff;
pub use super::hook::HookError;
pub use super::hook::command::CommandHook;
pub use super::hook::command::parse_event;
pub use super::hook::registry::{HookEntry, HookRegistry};
pub use super::hook::{
    ArbitratedVerdict, Hook, HookContext, HookEvent, HookPayload, HookPolicy, HookSource,
    HookVerdict, TrustState, arbitrate,
};
pub use super::lifecycle::{CompressResult, LlmSummarizer, compress_session};
pub use super::manifest::{
    CompressPolicy, HeuristicSummarizer, SummarizeError, Summarizer, build_manifest,
};
pub use super::projection::apply_manifest;
pub use super::prompt::SystemPrompt;
pub use super::prompt::extract::extraction_prompt;
pub use super::reducer::{
    HotPathReducer, ReduceCtx, ReducedOutput, ToolOutputReducer, TrustLevel, never_worse,
};
pub use super::status::{StatusSnapshot, UsageAccumulator};
pub use super::step::{AgentId, ApprovalDecision, ApprovalRequest, NextStep, TurnOutcome};
pub use super::thinking::{thinking_brief, turn_reasoning, turn_tool_summary};
pub use super::tool::{StubTool, ToolRegistry};
pub use super::tools::ask_user_question::AskUserQuestionTool;
pub use super::tools::conversation_search::ConversationSearchTool;
pub use super::tools::glob::GlobTool;
pub use super::tools::grep::GrepTool;
pub use super::tools::memory_add::MemoryAddTool;
pub use super::tools::todo::{TodoItem, TodoStatus, TodoWriteTool};
pub use super::tools::webfetch::WebFetchTool;
pub use super::tools::worktree_enter::EnterWorktreeTool;
pub use super::tools::worktree_exit::ExitWorktreeTool;
pub use super::tools::{BashTool, EditTool, MultiEditTool, ReadTool, WriteTool};
pub use super::turn_group::project_input_items;
pub use super::verify::{MakeCheckGate, VerifyFailure, VerifyGate};
pub use super::worktree_controller::WorktreeController;
