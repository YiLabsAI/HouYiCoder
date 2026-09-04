//! Hook spec parsing and policy settings. Split from the config root on
//! file-size grounds: the hook spec struct, the env-var and settings-file
//! parsers, and the hook policy settings form one cohesive unit that the
//! composition root consumes. The module stays private behind named
//! re-exports so the public surface is unchanged.

use crate::ENV_HOUYICODER_HOOKS;

/// One external command hook. The composition root spawns the program per
/// fire, pipes the hook context as JSON to stdin, and parses the verdict JSON
/// from stdout. Events are strings resolved against the runtime HookEvent
/// enum at the composition root, so this leaf crate stays free of any
/// dependency on the agent layer. A spec with an empty name or program is
/// dropped by the parser, mirroring the tool-server config: a typo must not
/// silently register a no-op hook.
///
/// The matcher, if_condition, shell, and timeout_secs fields carry the
/// hook-config format's filtering and execution options as raw strings.
/// This crate is a serde-only leaf with no pattern-matching dependency, so
/// the composition root parses them at build time and injects them into the
/// command hook. An unset field means no filtering (matcher) or the default
/// shell (shell).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HookSpec {
    /// A stable label the hook identifies itself by in verdicts and logs.
    pub name: String,
    /// Event names (PreToolUse, PostToolUse, PostToolUseFailure, ...). An
    /// unknown name skips this hook at registration, not at fire time.
    pub events: Vec<String>,
    /// The program to run (a binary name or path).
    pub program: String,
    /// Argv after the program.
    #[serde(default)]
    pub args: Vec<String>,
    /// A pattern matched against the event-specific query string (tool name
    /// for tool events). Empty or None means match all. The composition root
    /// parses this into a matcher; the raw string stays here for
    /// serialization and dedup.
    #[serde(default)]
    pub matcher: Option<String>,
    /// A permission-rule syntax condition (ToolName(content pattern)) matched
    /// before spawn. The initial implementation matches the tool name only;
    /// the composition root interprets the full syntax. Empty or None means
    /// no pre-filter.
    #[serde(default, rename = "if")]
    pub if_condition: Option<String>,
    /// The shell interpreter to run the command through. When set, the
    /// composition root maps the command string to a shell invocation
    /// (program becomes the shell, args become -c plus the command). None
    /// means the program and args fields are used directly (the legacy
    /// env-var format).
    #[serde(default)]
    pub shell: Option<String>,
    /// Per-hook timeout in seconds. None means the registry default applies.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// The list of external command hooks configured via env. An empty or unset
/// var yields an empty Vec so the composition root wires no command hook (the
/// engine still runs, the built-in fire points still fire, just no external
/// verdict source). A malformed value is also an empty Vec with a stderr
/// warning — the engine must not brick on a config typo.
pub fn resolve_hooks() -> Vec<HookSpec> {
    let raw = std::env::var(ENV_HOUYICODER_HOOKS).ok();
    match parse_hooks(raw.as_deref()) {
        Ok(list) => list,
        Err(msg) => {
            tracing::warn!("{ENV_HOUYICODER_HOOKS} ignored ({msg}); no command hooks wired");
            Vec::new()
        }
    }
}

/// Pure parser for the command hook list; testable without env mutation.
/// Accepts a JSON array of objects with name, events, program, and args
/// fields. An empty or unset value yields an empty list. A non-array value
/// is an error. Entries with an empty name or program are dropped: a hook
/// with no command cannot spawn, and silently registering a no-op hook
/// would mask the config typo.
pub(crate) fn parse_hooks(raw: Option<&str>) -> Result<Vec<HookSpec>, String> {
    let Some(body) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };
    let parsed: Vec<HookSpec> =
        serde_json::from_str(body).map_err(|e| format!("invalid json: {e}"))?;
    Ok(parsed
        .into_iter()
        .filter(|s| !s.name.is_empty() && !s.program.is_empty())
        .collect())
}

/// The hook-config format from a settings file: a top-level hooks object
/// keyed by event name, each value an array of matcher groups. A matcher
/// group carries an optional matcher pattern and a hooks array of command
/// entries. This is the per-source shape (one settings file's hooks
/// object), not the merged value, so the caller can tag each parsed spec
/// with its source for the trust gate.
///
/// A command entry with type command is mapped to a HookSpec: the command
/// string becomes a shell invocation (program = shell or sh, args = -c
/// plus the command). The if, shell, and timeout fields carry through as
/// raw strings for the composition root to interpret. A non-command type
/// is skipped (prompt, http, and agent hooks are deferred). An invalid
/// shape is skipped with a warning, not an error, so one bad entry does
/// not brick the whole hook set.
pub fn parse_hooks_from_settings(value: &serde_json::Value) -> Vec<HookSpec> {
    let Some(hooks_obj) = value.get("hooks").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut specs = Vec::new();
    for (event_name, matchers) in hooks_obj {
        let Some(matcher_arr) = matchers.as_array() else {
            tracing::warn!("hooks.{event_name} ignored: expected an array");
            continue;
        };
        for group in matcher_arr {
            let matcher = group
                .get("matcher")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let Some(hook_arr) = group.get("hooks").and_then(|v| v.as_array()) else {
                tracing::warn!("hooks.{event_name} group ignored: no hooks array");
                continue;
            };
            for hook in hook_arr {
                let hook_type = hook.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if hook_type != "command" {
                    if !hook_type.is_empty() {
                        tracing::debug!(
                            "hooks.{event_name} skipped non-command hook type {hook_type:?}"
                        );
                    }
                    continue;
                }
                let Some(command) = hook.get("command").and_then(|v| v.as_str()) else {
                    tracing::warn!("hooks.{event_name} command hook skipped: no command field");
                    continue;
                };
                if command.trim().is_empty() {
                    tracing::warn!("hooks.{event_name} command hook skipped: empty command");
                    continue;
                }
                let shell = hook
                    .get("shell")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let if_condition = hook
                    .get("if")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let timeout_secs = hook.get("timeout").and_then(|v| v.as_u64());
                let (program, args) = shell_invocation(shell.as_deref(), command);
                let name = format!("{event_name}:{command}");
                specs.push(HookSpec {
                    name,
                    events: vec![event_name.clone()],
                    program,
                    args,
                    matcher: matcher.clone(),
                    if_condition,
                    shell,
                    timeout_secs,
                });
            }
        }
    }
    specs
}

/// Map a shell type and command string to the program and argv the
/// launcher spawns. A None shell (the legacy env-var format) is unused
/// here since settings-sourced hooks always carry a command string; the
/// default shell is sh. The command string is passed as a single -c
/// argument so the shell interprets it (pipes, redirects, variable
/// expansion all work without the caller building a script file).
fn shell_invocation(shell: Option<&str>, command: &str) -> (String, Vec<String>) {
    let program = match shell {
        Some("bash") | Some("sh") | None => "sh",
        Some(other) => other,
    };
    (
        program.to_string(),
        vec!["-c".to_string(), command.to_string()],
    )
}

// ---- hook policy settings -------------------------------------------------

/// Hook policy settings read from the merged settings JSON. These control
/// which hook sources are active at runtime. The composition root maps
/// these to the engine HookPolicy enum.
///
/// disable_all_hooks: a managed setting that turns off all non-managed
/// hooks (user, project, local); managed hooks still run. This mirrors
/// the managed setting that disables user-configured hooks while keeping
/// policy-deployed hooks active.
///
/// allow_managed_hooks_only: a managed setting that restricts hooks to
/// managed/policy sources only. User, project, and local hooks are
/// skipped.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HookPolicySettings {
    /// Disable all non-managed hooks. Managed hooks still run.
    #[serde(default)]
    pub disable_all_hooks: bool,
    /// Only managed/policy hooks run; all other sources are skipped.
    #[serde(default)]
    pub allow_managed_hooks_only: bool,
}

/// Resolve hook policy settings from a merged settings JSON value. Reads
/// the top-level disableAllHooks and allowManagedHooksOnly fields. Both
/// default to false (all hooks enabled). When both are true,
/// allow_managed_hooks_only takes precedence (the stricter policy).
pub fn resolve_hook_policy_settings(value: &serde_json::Value) -> HookPolicySettings {
    let disable_all = value
        .get("disableAllHooks")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let managed_only = value
        .get("allowManagedHooksOnly")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    HookPolicySettings {
        disable_all_hooks: disable_all,
        allow_managed_hooks_only: managed_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hooks_empty() {
        assert!(parse_hooks(None).unwrap().is_empty());
        assert!(parse_hooks(Some("")).unwrap().is_empty());
        assert!(parse_hooks(Some("   ")).unwrap().is_empty());
    }

    #[test]
    fn test_parse_hooks_array() {
        let raw =
            r#"[{"name":"lint","events":["PreToolUse"],"program":"sh","args":["-c","echo hi"]}]"#;
        let list = parse_hooks(Some(raw)).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "lint");
        assert_eq!(list[0].events, vec!["PreToolUse"]);
        assert_eq!(list[0].program, "sh");
        assert_eq!(list[0].args, vec!["-c", "echo hi"]);
    }

    #[test]
    fn test_parse_hooks_invalid() {
        assert!(parse_hooks(Some("not json")).is_err());
        assert!(parse_hooks(Some(r#"[{"events":[],"program":"sh"}]"#)).is_err());
    }

    #[test]
    fn test_parse_hooks_round_trips() {
        let spec = HookSpec {
            name: "lint".into(),
            events: vec!["PreToolUse".into()],
            program: "sh".into(),
            args: vec!["-c".into(), "echo hi".into()],
            matcher: None,
            if_condition: None,
            shell: None,
            timeout_secs: None,
        };
        let s = serde_json::to_string(&spec).unwrap();
        let back: HookSpec = serde_json::from_str(&s).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn test_parse_hooks_drops_empty() {
        let raw = r#"[{"name":"","events":["PreToolUse"],"program":"sh"},{"name":"x","events":[],"program":""}]"#;
        let list = parse_hooks(Some(raw)).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn test_parse_settings_hooks_basic() {
        let val = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash|Edit",
                        "hooks": [
                            {"type": "command", "command": "echo lint", "timeout": 30}
                        ]
                    }
                ],
                "PostToolUse": [
                    {
                        "hooks": [
                            {"type": "command", "command": "echo done", "if": "Bash(git *)"}
                        ]
                    }
                ]
            }
        });
        let specs = parse_hooks_from_settings(&val);
        assert_eq!(specs.len(), 2);
        let pre = specs.iter().find(|s| s.events[0] == "PreToolUse").unwrap();
        assert_eq!(pre.matcher.as_deref(), Some("Bash|Edit"));
        assert_eq!(pre.timeout_secs, Some(30));
        assert_eq!(pre.program, "sh");
        assert_eq!(pre.args, vec!["-c", "echo lint"]);
        let post = specs.iter().find(|s| s.events[0] == "PostToolUse").unwrap();
        assert_eq!(post.if_condition.as_deref(), Some("Bash(git *)"));
        assert!(post.matcher.is_none());
    }

    #[test]
    fn test_parse_settings_hooks_noncmd() {
        let val = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "hooks": [
                            {"type": "prompt", "prompt": "check this"},
                            {"type": "command", "command": "echo ok"}
                        ]
                    }
                ]
            }
        });
        let specs = parse_hooks_from_settings(&val);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].args, vec!["-c", "echo ok"]);
    }

    #[test]
    fn test_parse_settings_hooks_empty() {
        let specs = parse_hooks_from_settings(&serde_json::json!({}));
        assert!(specs.is_empty());
        let specs = parse_hooks_from_settings(&serde_json::json!({"hooks": {}}));
        assert!(specs.is_empty());
    }

    #[test]
    fn test_resolve_hook_policy_defaults() {
        let p = resolve_hook_policy_settings(&serde_json::json!({}));
        assert!(!p.disable_all_hooks);
        assert!(!p.allow_managed_hooks_only);
    }

    #[test]
    fn test_hook_policy_disable_all() {
        let p = resolve_hook_policy_settings(&serde_json::json!({"disableAllHooks": true}));
        assert!(p.disable_all_hooks);
        assert!(!p.allow_managed_hooks_only);
    }

    #[test]
    fn test_hook_policy_managed_only() {
        let p = resolve_hook_policy_settings(&serde_json::json!({"allowManagedHooksOnly": true}));
        assert!(!p.disable_all_hooks);
        assert!(p.allow_managed_hooks_only);
    }
}
