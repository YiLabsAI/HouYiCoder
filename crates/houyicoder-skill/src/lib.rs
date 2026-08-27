//! Skill system: discovery, parsing, progressive disclosure, invocation,
//! and lifecycle for SKILL.md directory skills. Compatible with the
//! community skill ecosystem and the AgentSkill open specification.
//!
//! Depends on api (ports: Tool, ToolCtx, SkillSourceProvider trait),
//! context (TurnEventKind: SkillInvoked / SkillBody / SkillReturn),
//! protocol (wire types: SkillListing via FrontendEventKind),
//! async (PFut), session (SessionStore for SkillBody durable).

pub mod definition;
pub mod discover;
pub mod invoke;
pub mod parse;
