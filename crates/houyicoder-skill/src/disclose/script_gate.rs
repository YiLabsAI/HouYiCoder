//! Detect when a Bash command runs a skill-directory script so the agent
//! loop can route it through a per-script confirmation gate. Reference
//! reads are not flagged; this gate is for running a script. The match is
//! the canonical skill path followed by a script file path: forms that
//! carry no canonical path evade — a cd then a relative run, an unexpanded
//! env var, a relative path from another cwd. Defense-in-depth on top of
//! the sandbox, not a sole control.

use std::path::Path;

use super::resources::{ResourceKind, classify};
use crate::definition::SkillSource;

/// A skill-directory script a Bash command runs. Carries the skill name +
/// discovery source so the gate can decide always-allow (a managed or
/// user source) versus a per-time Ask (any other source), and the script's
/// path relative to the skill dir so the Ask can name what runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptRef {
    pub skill_name: String,
    pub script_rel_path: String,
    pub source: SkillSource,
}

/// Detect skill-directory script executions in a Bash command. Returns one
/// ScriptRef per skill whose directory path appears in the command and is
/// followed by a script path (under a scripts/ directory or carrying a
/// script extension). A reference-file read is not flagged: the resource
/// fence allows it, and this gate is for running a script. The skill set
/// is passed in so the detection stays a pure function of the command +
/// the registry view.
pub fn detect_skill_scripts(
    command: &str,
    skills: &[(String, SkillSource, &Path)],
) -> Vec<ScriptRef> {
    let mut out = Vec::new();
    for (name, source, dir) in skills {
        let dir_str = dir.to_string_lossy();
        // match_indices finds every occurrence, so a command running two
        // scripts from the same skill yields two refs (the gate is
        // per-script, not per-skill).
        for idx in command.match_indices(dir_str.as_ref()).map(|(i, _)| i) {
            let after = &command[idx + dir_str.len()..];
            // Path boundary: the dir must be followed by '/' so a shorter
            // skill name does not match a longer one (deploy vs
            // deployment). Without this a project script could mis-attribute
            // to a managed skill whose dir prefixes it and skip the Ask.
            if !after.starts_with('/') {
                continue;
            }
            let rel = after[1..]
                .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .next()
                .unwrap_or("");
            // Skip a bare directory (no file) or a trailing-slash dir
            // (python on a directory is an error, not a script run).
            if rel.is_empty() || rel.ends_with('/') {
                continue;
            }
            let file_name = Path::new(rel)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            // Only flag scripts: a reference note or template is a read,
            // not an execution.
            if !matches!(classify(rel, file_name), ResourceKind::Script) {
                continue;
            }
            out.push(ScriptRef {
                skill_name: name.clone(),
                script_rel_path: rel.to_string(),
                source: source.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests;
