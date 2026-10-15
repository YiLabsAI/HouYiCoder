//! Declares the public agent API surface.

pub use crate::agent::backbone::{
    BackboneDerivation, CompactBackbone, ConflictRate, GitWorkspaceProbe, StubWorkspaceProbe,
    WorkspaceProbe, derive_backbone, merge_summary, render_backbone_block,
};
pub use crate::agent::compaction::{CompactionOutcome, LlmSummarizer, RecordedCompaction};
pub use crate::agent::conditional_activation::{ConditionalActivation, ConditionalSkillActivator};
pub use crate::agent::context::{
    AssembledContext, CategoryBreakdown, ContextBreakdown, ContextBuilder, ContextMeasurement,
    GridCell, Section, SectionKind, Tokenizer, build_grid, stub_breakdown,
};
pub use crate::agent::diff::unified_diff;
pub use crate::agent::hook::HookError;
pub use crate::agent::hook::command::CommandHook;
pub use crate::agent::hook::command::parse_event;
pub use crate::agent::hook::fire::build_hook_fire;
pub use crate::agent::hook::registry::{HookEntry, HookId, HookRegistry};
pub use crate::agent::hook::{
    CombinedVerdict, Hook, HookContext, HookEvent, HookPayload, HookPolicy, HookSource,
    HookVerdict, ToolResult, combine_verdicts,
};
pub use crate::agent::manifest::{
    CompressPolicy, HeuristicSummarizer, SummarizeError, Summarizer, build_manifest,
};
pub use crate::agent::memory::{MemoryGateState, MemoryGates, MemoryRuntime};
pub use crate::agent::prompt::SystemPrompt;
pub use crate::agent::prompt::extract::extraction_prompt;
pub use crate::agent::reducer::{
    HotPathReducer, ReduceCtx, ReducedOutput, ToolOutputReducer, TrustLevel, never_worse,
};
pub use crate::agent::selection::apply_manifest;
pub use crate::agent::skill_hooks::SkillHookRegistrar;
pub use crate::agent::skill_reload::SkillReloadGuard;
pub use crate::agent::status::{StatusSnapshot, UsageAccumulator};
pub use crate::agent::step::{ApprovalDecision, ApprovalRequest, NextStep, TurnOutcome};
pub use crate::agent::thinking::thinking_brief;
pub use crate::agent::tool::{StubTool, ToolRegistry};
pub use crate::agent::tools::{
    AskUserQuestionTool, BashTool, ConversationSearchTool, DelegationTool, EditTool,
    EnterWorktreeTool, ExitWorktreeTool, GlobTool, GrepTool, MemoryAddTool, MultiEditTool,
    ReadTool, SkillTool, TodoItem, TodoStatus, TodoWriteTool, WebFetchTool, WriteTool,
};
pub use crate::agent::turn_group::assemble_model_input;
pub use crate::agent::verify::{MakeCheckGate, VerifyFailure, VerifyGate};
pub use crate::agent::worktree_controller::WorktreeController;
