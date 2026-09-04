//! Shared matcher and if-condition filtering for command hooks and skill
//! hooks. Both the external command hook and the skill command hook filter
//! before spawn: the matcher tests the event-specific query string (tool
//! name for tool events), and the if condition tests a permission-rule
//! syntax that combines the tool name with the tool input. The logic lives
//! here so both hook types share one implementation and one set of tests.

use super::{HookContext, HookPayload, SessionEndReason};

/// Whether the context's tool name satisfies a matcher pattern. Empty or
/// star matches all. A pattern of ascii letters, digits, underscores, and
/// pipes is an exact or pipe-separated list. Anything else is a regex. A
/// non-tool event (no tool name in the payload) never matches.
pub(crate) fn matcher_passes(ctx: &HookContext, matcher: &str) -> bool {
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    let Some(query) = match_query(ctx) else {
        return false;
    };
    if matcher
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|')
    {
        if matcher.contains('|') {
            return matcher
                .split('|')
                .any(|p| p.trim().eq_ignore_ascii_case(&query));
        }
        return matcher.eq_ignore_ascii_case(&query);
    }
    match build_matcher_regex(matcher) {
        Some(re) => re.is_match(&query),
        None => false,
    }
}

/// Compile a matcher pattern as a case-insensitive regex. The matcher
/// only ever tests an identifier-shaped query (a tool name, a session
/// start kind, a compact trigger, a changed file's basename), and tool
/// names are lowercase in the engine while hook configs conventionally
/// capitalize them. Matching case-sensitively here would make an exact
/// matcher and an equivalent anchored alternation disagree, so both
/// branches ignore case. Returns None on an invalid pattern, which
/// fails the matcher closed.
fn build_matcher_regex(matcher: &str) -> Option<regex::Regex> {
    match regex::RegexBuilder::new(matcher)
        .case_insensitive(true)
        .build()
    {
        Ok(re) => Some(re),
        Err(_) => {
            tracing::warn!(matcher = %matcher, "hook matcher is not valid regex");
            None
        }
    }
}

/// Compile a matcher pattern once at build time. Returns None for
/// exact/pipe matchers (no regex needed) or invalid regex (the hot path
/// warns and fails closed). The caller passes the compiled regex to
/// matcher_passes_compiled on each fire.
pub(crate) fn compile_matcher(matcher: &str) -> Option<regex::Regex> {
    if matcher.is_empty() || matcher == "*" {
        return None;
    }
    if matcher
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|')
    {
        return None;
    }
    build_matcher_regex(matcher)
}

/// Matcher check with a pre-compiled regex. The raw string is still
/// needed for exact/pipe matching (no regex involved). A None regex
/// means the matcher is exact/pipe/wildcard — fall back to string logic.
pub(crate) fn matcher_passes_compiled(
    ctx: &HookContext,
    matcher: &str,
    compiled: Option<&regex::Regex>,
) -> bool {
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    let Some(query) = match_query(ctx) else {
        return false;
    };
    if matcher
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|')
    {
        if matcher.contains('|') {
            return matcher
                .split('|')
                .any(|p| p.trim().eq_ignore_ascii_case(&query));
        }
        return matcher.eq_ignore_ascii_case(&query);
    }
    match compiled {
        Some(re) => re.is_match(&query),
        None => false,
    }
}

/// Whether the context satisfies a Tool(pattern) if-rule. The tool name
/// must match; a bare Tool (no parens) passes on tool match. A pattern is
/// glob-matched (star and question) against the tool's primary content
/// field — the field the tool's permission rules also key on. For Bash
/// this is input.command; for Write/Edit input.path; for WebFetch
/// input.url; for Skill input.skill. Unknown tools fall back to walking
/// all string values in the input JSON (an over-approximation so a new
/// tool is not silently un-gated). A non-tool event never passes.
pub(crate) fn if_rule_passes(ctx: &HookContext, rule: &str) -> bool {
    let (rule_tool, pattern) = parse_if_rule(rule);
    let Some(tool) = tool_name(ctx) else {
        return false;
    };
    if !tool.eq_ignore_ascii_case(rule_tool) {
        return false;
    }
    let Some(pattern) = pattern else {
        return true;
    };
    let Some(input) = tool_input(ctx) else {
        return false;
    };
    // Try the tool's primary content field first (exact match on the
    // field the tool's rules key on). If the tool is unknown, walk all
    // strings as a conservative over-approximation.
    if let Some(content) = primary_content_field(tool, &input) {
        let Some(re) = glob_to_regex(pattern) else {
            return false;
        };
        return re.is_match(content);
    }
    glob_matches_any(&input, pattern)
}

/// Compile the glob pattern inside a Tool(pattern) if-rule once at build
/// time. Returns None if the rule has no pattern (bare Tool) or the glob
/// fails to compile. The caller passes the compiled regex to
/// if_rule_passes_compiled on each fire.
pub(crate) fn compile_if_pattern(rule: &str) -> Option<regex::Regex> {
    let (_tool, pattern) = parse_if_rule(rule);
    pattern.and_then(glob_to_regex)
}

/// If-rule check with a pre-compiled glob regex. The raw rule string is
/// still needed for the tool-name match and the bare-Tool (no pattern)
/// case. A None compiled regex means no pattern or a bad glob — the
/// caller falls back to the uncompiled path for bare-Tool, or fails
/// closed for a bad glob.
pub(crate) fn if_rule_passes_compiled(
    ctx: &HookContext,
    rule: &str,
    compiled: Option<&regex::Regex>,
) -> bool {
    let (rule_tool, pattern) = parse_if_rule(rule);
    let Some(tool) = tool_name(ctx) else {
        return false;
    };
    if !tool.eq_ignore_ascii_case(rule_tool) {
        return false;
    }
    let Some(pattern) = pattern else {
        return true;
    };
    let Some(input) = tool_input(ctx) else {
        return false;
    };
    if let Some(content) = primary_content_field(tool, &input) {
        return match compiled {
            Some(re) => re.is_match(content),
            None => false,
        };
    }
    // Unknown tool: walk all strings. The compiled regex is still valid
    // here (it is the glob pattern), so reuse it instead of recompiling.
    match compiled {
        Some(re) => !walk_strings(&input, |s| !re.is_match(s)),
        None => glob_matches_any(&input, pattern),
    }
}

/// Extract the event-specific query string the matcher tests against.
/// Tool events use the tool name; SessionStart uses "startup" or "resume";
/// FileChanged uses the basename of the first changed path; PreCompact
/// uses the trigger; Setup uses "setup"; UserPromptSubmit uses the prompt
/// text. Returns None for events with no meaningful query string (the
/// matcher never matches them, same as an empty matcher).
pub(crate) fn match_query(ctx: &HookContext) -> Option<String> {
    match &ctx.payload {
        HookPayload::PreToolUse { tool_name, .. }
        | HookPayload::PostToolUse { tool_name, .. }
        | HookPayload::PostToolUseFailure { tool_name, .. } => Some(tool_name.clone()),
        HookPayload::SessionStart { resumed } => {
            Some(if *resumed { "resume" } else { "startup" }.into())
        }
        HookPayload::FileChanged { paths } => paths
            .first()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned()),
        HookPayload::PreCompact { trigger, .. } | HookPayload::PostCompact { trigger, .. } => {
            Some(trigger.as_str().into())
        }
        HookPayload::Setup => Some("setup".into()),
        HookPayload::PermissionRequest { tool_name, .. }
        | HookPayload::PermissionDenied { tool_name, .. } => Some(tool_name.clone()),
        HookPayload::SessionEnd { reason } => Some(match reason {
            SessionEndReason::Clear => "clear".into(),
            SessionEndReason::Resume => "resume".into(),
            SessionEndReason::Logout => "logout".into(),
            SessionEndReason::Other(s) => s.clone(),
        }),
        HookPayload::SubagentStart { agent_type, .. }
        | HookPayload::SubagentStop { agent_type, .. } => Some(agent_type.clone()),
        HookPayload::InstructionsLoaded { source } => Some(source.clone()),
        // Free-text queries. The matcher is normally an identifier-shaped
        // kind selector, but these events carry no kind field, so the
        // message body is the only thing worth selecting on. A regex
        // matcher can substring-match it; an exact matcher only fires on
        // a whole-body equality, which is rarely what a user wants.
        HookPayload::UserPromptSubmit { prompt } => Some(prompt.clone()),
        HookPayload::Notification { message } => Some(message.clone()),
        HookPayload::StopFailure { error } => Some(error.clone()),
        _ => None,
    }
}

/// Extract the tool name from a tool-lifecycle payload. Returns None for
/// non-tool events (the if-rule's Tool part is a tool name, so it only
/// applies to tool events).
fn tool_name(ctx: &HookContext) -> Option<&str> {
    match &ctx.payload {
        HookPayload::PreToolUse { tool_name, .. }
        | HookPayload::PostToolUse { tool_name, .. }
        | HookPayload::PostToolUseFailure { tool_name, .. } => Some(tool_name),
        _ => None,
    }
}

/// Extract the tool input from a tool-lifecycle payload. For PreToolUse
/// and PostToolUse, this is the tool's input JSON. For PostToolUseFailure,
/// the error string is wrapped in a JSON object so the if-rule pattern can
/// match against it. Returns None for non-tool events.
fn tool_input(ctx: &HookContext) -> Option<serde_json::Value> {
    match &ctx.payload {
        HookPayload::PreToolUse { input, .. } | HookPayload::PostToolUse { input, .. } => {
            Some(input.clone())
        }
        HookPayload::PostToolUseFailure { error, .. } => Some(serde_json::json!({"error": error})),
        _ => None,
    }
}

/// Split a Tool(pattern) rule into (tool, optional pattern). A bare Tool
/// has no parens.
fn parse_if_rule(rule: &str) -> (&str, Option<&str>) {
    if let Some(open) = rule.find('(') {
        let tool = rule[..open].trim();
        let inner = rule[open + 1..].trim_end_matches(')').trim();
        (tool, Some(inner))
    } else {
        (rule.trim(), None)
    }
}

/// The primary content field for a tool — the field the tool's permission
/// rules also key on. Returns None for unknown tools so the caller falls
/// back to walking all strings (a conservative over-approximation that
/// never silently un-gates a new tool).
fn primary_content_field<'a>(tool: &str, input: &'a serde_json::Value) -> Option<&'a str> {
    let key = match tool.to_ascii_lowercase().as_str() {
        "bash" | "sh" | "exec" | "shell" => "command",
        "write" | "edit" | "multiedit" | "patch" | "str_replace" => "path",
        "webfetch" | "netfetch" | "fetch" | "curl" | "wget" => "url",
        "skill" => "skill",
        _ => return None,
    };
    input.get(key).and_then(|v| v.as_str())
}

/// Glob-match a pattern against any string value in the input JSON. The
/// pattern supports star and question (translated to regex); other
/// characters are literal. Anchored as a full match. Walks the JSON
/// lazily and returns on the first match so a large input does not
/// require collecting every string before testing.
fn glob_matches_any(input: &serde_json::Value, pattern: &str) -> bool {
    let Some(re) = glob_to_regex(pattern) else {
        return false;
    };
    // walk_strings returns false if the callback returned false (early
    // exit on match). Invert: a false from walk means a match was found.
    !walk_strings(input, |s| {
        if re.is_match(s) {
            // Stop the walk: the pattern matched.
            false
        } else {
            true
        }
    })
}

fn glob_to_regex(pattern: &str) -> Option<regex::Regex> {
    let mut out = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            _ => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    out.push('$');
    regex::Regex::new(&out).ok()
}

/// Walk every string value reachable in the JSON (object values, array
/// elements, nested). The callback returns false to stop the walk early
/// (a match), true to continue. Returns false if the walk was stopped
/// early (callback returned false), true if the walk completed without
/// stopping. Non-string leaves are ignored. Borrows the JSON — no
/// allocation unless the callback allocates.
fn walk_strings<F>(value: &serde_json::Value, mut f: F) -> bool
where
    F: FnMut(&str) -> bool,
{
    fn walk<F>(value: &serde_json::Value, f: &mut F) -> bool
    where
        F: FnMut(&str) -> bool,
    {
        match value {
            serde_json::Value::String(s) => f(s),
            serde_json::Value::Array(a) => a.iter().all(|v| walk(v, f)),
            serde_json::Value::Object(o) => o.values().all(|v| walk(v, f)),
            _ => true,
        }
    }
    walk(value, &mut f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use houyicoder_context::SessionId;

    fn ctx_pre(tool: &str) -> HookContext {
        HookContext {
            event: super::super::HookEvent::PreToolUse,
            payload: HookPayload::PreToolUse {
                tool_name: tool.into(),
                input: serde_json::json!({"command": "git push"}),
                backfilled_input: None,
            },
            session: SessionId::new(),
        }
    }

    #[test]
    fn test_matcher_empty_matches_all() {
        assert!(matcher_passes(&ctx_pre("Bash"), ""));
    }

    #[test]
    fn test_matcher_star_matches_all() {
        assert!(matcher_passes(&ctx_pre("Bash"), "*"));
    }

    #[test]
    fn test_matcher_exact() {
        assert!(matcher_passes(&ctx_pre("Bash"), "Bash"));
        assert!(!matcher_passes(&ctx_pre("Edit"), "Bash"));
    }

    #[test]
    fn test_matcher_case_insensitive() {
        // Tool names are lowercase in the engine; user config uses
        // capitalized names. The matcher compares case-insensitively.
        assert!(matcher_passes(&ctx_pre("bash"), "Bash"));
        assert!(matcher_passes(&ctx_pre("Bash"), "bash"));
        assert!(matcher_passes(&ctx_pre("bash"), "BASH"));
    }

    #[test]
    fn test_matcher_regex_case_insensitive() {
        // The regex branch must be case-insensitive too, or a config
        // written against the capitalized ecosystem names silently
        // stops matching the moment the user switches from an exact
        // matcher to an anchored alternation.
        assert!(matcher_passes(&ctx_pre("bash"), "^(Bash|Edit)$"));
        assert!(matcher_passes(&ctx_pre("edit"), "^(Bash|Edit)$"));
        assert!(!matcher_passes(&ctx_pre("read"), "^(Bash|Edit)$"));
        let re = compile_matcher("^(Bash|Edit)$").expect("regex matcher compiles");
        assert!(matcher_passes_compiled(
            &ctx_pre("bash"),
            "^(Bash|Edit)$",
            Some(&re)
        ));
    }

    #[test]
    fn test_matcher_pipe() {
        assert!(matcher_passes(&ctx_pre("Bash"), "Bash|Edit"));
        assert!(matcher_passes(&ctx_pre("Edit"), "Bash|Edit"));
        assert!(!matcher_passes(&ctx_pre("Read"), "Bash|Edit"));
    }

    #[test]
    fn test_matcher_regex() {
        assert!(matcher_passes(&ctx_pre("Bash"), "^(Bash|Edit)$"));
        assert!(matcher_passes(&ctx_pre("Edit"), "^(Bash|Edit)$"));
        assert!(!matcher_passes(&ctx_pre("Read"), "^(Bash|Edit)$"));
    }

    #[test]
    fn test_if_rule_tool_only() {
        assert!(if_rule_passes(&ctx_pre("Bash"), "Bash"));
        assert!(!if_rule_passes(&ctx_pre("Edit"), "Bash"));
    }

    #[test]
    fn test_if_rule_case_insensitive() {
        assert!(if_rule_passes(&ctx_pre("bash"), "Bash"));
        assert!(if_rule_passes(&ctx_pre("Bash"), "bash"));
    }

    #[test]
    fn test_if_rule_with_pattern() {
        assert!(if_rule_passes(&ctx_pre("Bash"), "Bash(git *)"));
        assert!(!if_rule_passes(&ctx_pre("Bash"), "Bash(npm *)"));
    }

    #[test]
    fn test_if_rule_content_bash() {
        // Bash matches input.command, not other string fields.
        let ctx = HookContext {
            event: super::super::HookEvent::PreToolUse,
            payload: HookPayload::PreToolUse {
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "git push", "cwd": "/tmp"}),
                backfilled_input: None,
            },
            session: SessionId::new(),
        };
        assert!(if_rule_passes(&ctx, "Bash(git *)"));
        assert!(!if_rule_passes(&ctx, "Bash(/tmp*)"));
    }

    #[test]
    fn test_if_rule_content_write() {
        // Write matches input.path.
        let ctx = HookContext {
            event: super::super::HookEvent::PreToolUse,
            payload: HookPayload::PreToolUse {
                tool_name: "Write".into(),
                input: serde_json::json!({"path": "/etc/passwd", "content": "root"}),
                backfilled_input: None,
            },
            session: SessionId::new(),
        };
        assert!(if_rule_passes(&ctx, "Write(/etc/*)"));
        assert!(!if_rule_passes(&ctx, "Write(root*)"));
    }

    #[test]
    fn test_if_unknown_walks_strings() {
        // Unknown tool falls back to walking all strings.
        let ctx = HookContext {
            event: super::super::HookEvent::PreToolUse,
            payload: HookPayload::PreToolUse {
                tool_name: "CustomTool".into(),
                input: serde_json::json!({"data": "hello world"}),
                backfilled_input: None,
            },
            session: SessionId::new(),
        };
        assert!(if_rule_passes(&ctx, "CustomTool(hello*)"));
    }

    #[test]
    fn test_nontool_event_no_match() {
        let ctx = HookContext {
            event: super::super::HookEvent::SessionStart,
            payload: HookPayload::SessionStart { resumed: false },
            session: SessionId::new(),
        };
        assert!(!matcher_passes(&ctx, "Bash"));
    }

    #[test]
    fn test_compile_matcher_exact() {
        assert!(compile_matcher("Bash").is_none());
        assert!(compile_matcher("Bash|Edit").is_none());
        assert!(compile_matcher("").is_none());
        assert!(compile_matcher("*").is_none());
    }

    #[test]
    fn test_compile_matcher_regex() {
        assert!(compile_matcher("^(Bash|Edit)$").is_some());
    }

    #[test]
    fn test_compile_matcher_bad_regex() {
        assert!(compile_matcher("[invalid").is_none());
    }

    #[test]
    fn test_matcher_compiled_exact() {
        let ctx = ctx_pre("Bash");
        assert!(matcher_passes_compiled(&ctx, "Bash", None));
        assert!(!matcher_passes_compiled(&ctx, "Edit", None));
    }

    #[test]
    fn test_matcher_compiled_regex() {
        let re = compile_matcher("^(Bash|Edit)$").unwrap();
        assert!(matcher_passes_compiled(
            &ctx_pre("Bash"),
            "^(Bash|Edit)$",
            Some(&re)
        ));
        assert!(!matcher_passes_compiled(
            &ctx_pre("Read"),
            "^(Bash|Edit)$",
            Some(&re)
        ));
    }

    #[test]
    fn test_compile_if_bare() {
        assert!(compile_if_pattern("Bash").is_none());
    }

    #[test]
    fn test_compile_if_glob() {
        assert!(compile_if_pattern("Bash(git *)").is_some());
    }

    #[test]
    fn test_if_rule_compiled_bash() {
        let re = compile_if_pattern("Bash(git *)").unwrap();
        let ctx = HookContext {
            event: super::super::HookEvent::PreToolUse,
            payload: HookPayload::PreToolUse {
                tool_name: "Bash".into(),
                input: serde_json::json!({"command": "git push"}),
                backfilled_input: None,
            },
            session: SessionId::new(),
        };
        assert!(if_rule_passes_compiled(&ctx, "Bash(git *)", Some(&re)));
    }

    #[test]
    fn test_match_query_session_start() {
        let ctx = HookContext {
            event: super::super::HookEvent::SessionStart,
            payload: HookPayload::SessionStart { resumed: false },
            session: SessionId::new(),
        };
        assert_eq!(match_query(&ctx).as_deref(), Some("startup"));
        assert!(matcher_passes(&ctx, "startup"));
        assert!(!matcher_passes(&ctx, "resume"));
    }

    #[test]
    fn test_match_query_session_resume() {
        let ctx = HookContext {
            event: super::super::HookEvent::SessionStart,
            payload: HookPayload::SessionStart { resumed: true },
            session: SessionId::new(),
        };
        assert_eq!(match_query(&ctx).as_deref(), Some("resume"));
        assert!(matcher_passes(&ctx, "resume"));
    }

    #[test]
    fn test_match_query_setup() {
        let ctx = HookContext {
            event: super::super::HookEvent::Setup,
            payload: HookPayload::Setup,
            session: SessionId::new(),
        };
        assert_eq!(match_query(&ctx).as_deref(), Some("setup"));
        assert!(matcher_passes(&ctx, "setup"));
    }

    #[test]
    fn test_match_query_file_changed() {
        let ctx = HookContext {
            event: super::super::HookEvent::FileChanged,
            payload: HookPayload::FileChanged {
                paths: vec!["/tmp/foo.txt".into()],
            },
            session: SessionId::new(),
        };
        assert_eq!(match_query(&ctx).as_deref(), Some("foo.txt"));
        assert!(matcher_passes(&ctx, "foo.txt"));
        assert!(matcher_passes(&ctx, r".*\.txt"));
    }

    #[test]
    fn test_match_query_subagent() {
        // A matcher selects which subagent kind the hook fires for.
        let ctx = HookContext {
            event: super::super::HookEvent::SubagentStop,
            payload: HookPayload::SubagentStop {
                agent_id: houyicoder_context::AgentId("a1".into()),
                agent_type: "code-reviewer".into(),
                status: "completed".into(),
                last_text: None,
            },
            session: SessionId::new(),
        };
        assert_eq!(match_query(&ctx).as_deref(), Some("code-reviewer"));
        assert!(matcher_passes(&ctx, "code-reviewer"));
        assert!(!matcher_passes(&ctx, "planner"));
    }

    #[test]
    fn test_match_query_session_end() {
        let ctx = HookContext {
            event: super::super::HookEvent::SessionEnd,
            payload: HookPayload::SessionEnd {
                reason: SessionEndReason::Logout,
            },
            session: SessionId::new(),
        };
        assert_eq!(match_query(&ctx).as_deref(), Some("logout"));
        assert!(matcher_passes(&ctx, "logout"));
        assert!(!matcher_passes(&ctx, "clear"));
    }

    #[test]
    fn test_match_query_compact_trigger() {
        // The trigger string comes from the enum's own accessor, not a
        // debug rendering, so renaming the variant cannot silently
        // change what a configured matcher selects.
        let ctx = HookContext {
            event: super::super::HookEvent::PreCompact,
            payload: HookPayload::PreCompact {
                trigger: super::super::CompactTrigger::Auto,
                pre_compact_event_count: 0,
                pre_compact_token_estimate: 0,
            },
            session: SessionId::new(),
        };
        assert_eq!(match_query(&ctx).as_deref(), Some("auto"));
        assert!(matcher_passes(&ctx, "auto"));
        assert!(!matcher_passes(&ctx, "manual"));
    }

    #[test]
    fn test_match_query_unmapped_event() {
        // An event with no meaningful kind field matches only the
        // wildcard forms, never a named matcher.
        let ctx = HookContext {
            event: super::super::HookEvent::PreSelect,
            payload: HookPayload::PreSelect {
                current_token_estimate: 0,
            },
            session: SessionId::new(),
        };
        assert_eq!(match_query(&ctx), None);
        assert!(matcher_passes(&ctx, ""));
        assert!(matcher_passes(&ctx, "*"));
        assert!(!matcher_passes(&ctx, "anything"));
    }
}
