//! Skill system: discovery, parsing, and body preparation for SKILL.md
//! directory skills. Compatible with the community skill ecosystem and the
//! AgentSkill open specification.
//!
//! This crate is a pure data-processing layer: it reads SKILL.md files from
//! disk, parses the YAML frontmatter, discovers skills across the configured
//! scan paths, and prepares the body text (argument + variable substitution)
//! for invocation. It has no engine or runtime dependencies — the Tool
//! implementation that calls into these functions lives in the engine layer.

pub mod definition;
pub mod discover;
pub mod invoke;
pub mod parse;
