//! Agent definition loader: frontmatter parsing, directory scan, precedence.
//!
//! Turns agent definition files (an agents/ directory of .md) into
//! AgentDefinition values and layers them by source precedence. Frontmatter
//! is a YAML-ish block of key:value lines (scalars and comma-separated
//! lists); no YAML dependency, because the agent field set is small and a
//! line parser keeps the failure surface transparent. A corrupt file is
//! skipped, not fatal -- one bad definition must not blank the registry.

use std::path::Path;

use houyicoder_protocol::llm::EffortLevel;

use super::registry::{AgentDefinition, IsolationMode, MemoryScope, PromptSource};

/// Where a definition came from. Drives merge precedence: a higher-ordered
/// source overrides a lower one for the same subagent_type. Plugin and
/// managed layers are reserved for later; the runtime loads built-in, user,
/// and project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefinitionSource {
    BuiltIn,
    User,
    Project,
    Managed,
}

impl DefinitionSource {
    fn order(self) -> u8 {
        match self {
            Self::BuiltIn => 0,
            Self::User => 1,
            Self::Project => 2,
            Self::Managed => 3,
        }
    }
}

/// Why a definition file did not load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    MissingName,
    Corrupt(String),
}

/// Parse one definition file's text into an AgentDefinition. Frontmatter is
/// delimited by lines of three dashes; the body after the closing fence
/// becomes the system prompt. Unknown keys are ignored.
pub fn parse_agent_definition(text: &str) -> Result<AgentDefinition, LoadError> {
    let mut lines = text.lines();
    let first = lines.next().map(|l| l.trim()).unwrap_or("");
    if first != "---" {
        return Err(LoadError::Corrupt("frontmatter must open with ---".into()));
    }
    let mut fields: Vec<(&str, &str)> = Vec::new();
    let mut body_start = None;
    let collected: Vec<&str> = lines.collect();
    for (i, line) in collected.iter().enumerate() {
        if line.trim() == "---" {
            body_start = Some(i + 1);
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            fields.push((k.trim(), v.trim()));
        }
    }
    if body_start.is_none() {
        return Err(LoadError::Corrupt("frontmatter never closed".into()));
    }

    let get = |key: &str| fields.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
    let name = get("name").ok_or(LoadError::MissingName)?.to_string();
    let when_to_use = get("description").unwrap_or("").to_string();
    let tools = get("tools").map(split_commas);
    let disallowed_tools = get("disallowed_tools")
        .or_else(|| get("disallowedTools"))
        .map(split_commas)
        .unwrap_or_default();
    let model = get("model").map(str::to_string);
    let effort = get("effort").and_then(parse_effort);
    let permission_mode = get("permission_mode")
        .or_else(|| get("permissionMode"))
        .map(str::to_string);
    let max_turns = get("max_turns")
        .or_else(|| get("maxTurns"))
        .and_then(|v| v.parse::<u32>().ok());
    let skills = get("skills").map(split_commas).unwrap_or_default();
    let mcp_servers = get("mcp_servers")
        .or_else(|| get("mcpServers"))
        .map(split_commas)
        .unwrap_or_default();
    let hooks = get("hooks").map(split_commas).unwrap_or_default();
    let initial_prompt = get("initial_prompt")
        .or_else(|| get("initialPrompt"))
        .map(str::to_string);
    let memory = get("memory")
        .and_then(parse_memory_scope)
        .unwrap_or_default();
    let isolation = get("isolation")
        .and_then(parse_isolation)
        .unwrap_or_default();
    let omit_project_context = get("omit_project_context")
        .or_else(|| get("omitProjectContext"))
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let color = get("color").map(str::to_string);

    let body = match body_start {
        Some(idx) if idx < collected.len() => collected[idx..].join("\n"),
        _ => String::new(),
    };

    Ok(AgentDefinition {
        subagent_type: name,
        when_to_use,
        tools,
        disallowed_tools,
        model,
        effort,
        permission_mode,
        max_turns,
        skills,
        mcp_servers,
        hooks,
        initial_prompt,
        memory,
        isolation,
        omit_project_context,
        color,
        system_prompt: PromptSource::Owned(body),
    })
}

fn split_commas(v: &str) -> Vec<String> {
    v.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_effort(v: &str) -> Option<EffortLevel> {
    match v.to_ascii_lowercase().as_str() {
        "low" => Some(EffortLevel::Low),
        "medium" => Some(EffortLevel::Medium),
        "high" => Some(EffortLevel::High),
        _ => None,
    }
}

fn parse_memory_scope(v: &str) -> Option<MemoryScope> {
    match v.to_ascii_lowercase().as_str() {
        "disabled" => Some(MemoryScope::Disabled),
        "user" => Some(MemoryScope::User),
        "project" => Some(MemoryScope::Project),
        "local" => Some(MemoryScope::Local),
        _ => None,
    }
}

fn parse_isolation(v: &str) -> Option<IsolationMode> {
    match v.to_ascii_lowercase().as_str() {
        "none" => Some(IsolationMode::None),
        "worktree" => Some(IsolationMode::Worktree),
        _ => None,
    }
}

/// Collects definitions across sources and merges by precedence. Higher-
/// ordered sources override earlier ones for the same subagent_type; distinct
/// types coexist. Output order is stable by first-seen type, so callers that
/// rely on deterministic ordering (catalog injection) are not surprised by a
/// reshuffle when a later source overrides.
#[derive(Default)]
pub struct Loader {
    entries: Vec<(DefinitionSource, AgentDefinition)>,
}

impl Loader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, source: DefinitionSource, def: AgentDefinition) {
        self.entries.push((source, def));
    }

    pub fn merge(self) -> Vec<AgentDefinition> {
        let mut keyed = self.entries;
        keyed.sort_by_key(|(s, _)| s.order());
        let mut by_type: std::collections::BTreeMap<String, AgentDefinition> =
            std::collections::BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        for (_, def) in keyed {
            if !by_type.contains_key(&def.subagent_type) {
                order.push(def.subagent_type.clone());
            }
            by_type.insert(def.subagent_type.clone(), def);
        }
        order
            .into_iter()
            .map(|t| by_type.remove(&t).expect("key present in first-seen order"))
            .collect()
    }
}

/// Scan a directory for .md files and parse each, tagging results with the
/// given source. Corrupt files are skipped and reported; a single bad file
/// never blanks the directory.
pub fn load_dir(
    dir: &Path,
    source: DefinitionSource,
) -> (Vec<(DefinitionSource, AgentDefinition)>, Vec<String>) {
    let mut defs = Vec::new();
    let mut failures = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (defs, failures);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            failures.push(path.display().to_string());
            continue;
        };
        match parse_agent_definition(&text) {
            Ok(def) => defs.push((source, def)),
            Err(_) => failures.push(path.display().to_string()),
        }
    }
    (defs, failures)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::{DefinitionSource, LoadError, Loader, load_dir, parse_agent_definition};
    use crate::agent::multi_agent::registry::PromptSource;

    #[test]
    fn test_parse_minimal_definition() {
        let text = "---\nname: researcher\ndescription: reads sources and summarizes\n---\nDo the research.";
        let def = parse_agent_definition(text).expect("minimal frontmatter must parse");
        assert_eq!(def.subagent_type, "researcher");
        assert_eq!(def.when_to_use, "reads sources and summarizes");
        assert!(def.tools.is_none(), "absent tools means inherit all");
        assert_eq!(
            def.system_prompt,
            PromptSource::Owned("Do the research.".to_string())
        );
    }

    #[test]
    fn test_parse_tools_comma_list() {
        let text = "---\nname: reader\ntools: Read, Grep, Glob\n---\nbody";
        let def = parse_agent_definition(text).unwrap();
        assert_eq!(
            def.tools,
            Some(vec![
                "Read".to_string(),
                "Grep".to_string(),
                "Glob".to_string()
            ])
        );
    }

    #[test]
    fn test_parse_unknown_fields_ignored() {
        let text = "---\nname: x\ndescription: y\nfutureField: anything\n---\nbody";
        let def = parse_agent_definition(text).unwrap();
        assert_eq!(def.subagent_type, "x");
    }

    #[test]
    fn test_parse_omit_project_context() {
        let on =
            parse_agent_definition("---\nname: scanner\nomit_project_context: true\n---\nbody")
                .unwrap();
        assert!(on.omit_project_context);

        let off = parse_agent_definition("---\nname: other\n---\nbody").unwrap();
        assert!(!off.omit_project_context, "absent key defaults to false");

        let camel = parse_agent_definition("---\nname: camel\nomitProjectContext: true\n---\nbody")
            .unwrap();
        assert!(camel.omit_project_context, "camelCase alias accepted");
    }

    #[test]
    fn test_merge_project_overrides_user() {
        let mut loader = Loader::new();
        loader.add(
            DefinitionSource::User,
            parse_agent_definition("---\nname: shared\ndescription: user version\n---\nu").unwrap(),
        );
        loader.add(
            DefinitionSource::Project,
            parse_agent_definition("---\nname: shared\ndescription: project version\n---\np")
                .unwrap(),
        );
        let merged = loader.merge();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].when_to_use, "project version");
    }

    #[test]
    fn test_merge_keeps_distinct() {
        let mut loader = Loader::new();
        loader.add(
            DefinitionSource::User,
            parse_agent_definition("---\nname: a\ndescription: ua\n---\nua").unwrap(),
        );
        loader.add(
            DefinitionSource::Project,
            parse_agent_definition("---\nname: b\ndescription: pb\n---\npb").unwrap(),
        );
        let merged = loader.merge();
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_parse_missing_name_errors() {
        let text = "---\ndescription: no name\n---\nbody";
        assert!(matches!(
            parse_agent_definition(text),
            Err(LoadError::MissingName)
        ));
    }

    #[test]
    fn test_load_dir_skips_corrupt() {
        let dir = unique_temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("good.md"),
            "---\nname: good\ndescription: ok\n---\nbody",
        )
        .unwrap();
        fs::write(dir.join("bad.md"), "no frontmatter here").unwrap();
        let (defs, failures) = load_dir(&dir, DefinitionSource::Project);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].1.subagent_type, "good");
        assert!(failures.contains(&dir.join("bad.md").display().to_string()));
        fs::remove_dir_all(&dir).ok();
    }

    fn unique_temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("houyi-loader-{}-{}", std::process::id(), n))
    }
}
