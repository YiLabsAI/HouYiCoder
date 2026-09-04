//! Shared matcher and if-condition filtering for command hooks and skill
//! hooks. Both the external command hook and the skill command hook filter
//! before spawn: the matcher tests the event-specific query string (tool
//! name for tool events), and the if condition tests a permission-rule
//! syntax that combines the tool name with the tool input. The logic lives
//! here so both hook types share one implementation and one set of tests.

use super::{HookContext, HookPayload};

/// Whether the context's tool name satisfies a matcher pattern. Empty or
/// star matches all. A pattern of ascii letters, digits, underscores, and
/// pipes is an exact or pipe-separated list. Anything else is a regex. A
/// non-tool event (no tool name in the payload) never matches.
pub(crate) fn matcher_passes(ctx: &HookContext, matcher: &str) -> bool {
    if matcher.is_empty() || matcher == "*" {
        return true;
    }
    let Some(tool) = tool_name(ctx) else {
        return false;
    };
    if matcher
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '|')
    {
        if matcher.contains('|') {
            return matcher.split('|').any(|p| p.trim() == tool);
        }
        return matcher == tool;
    }
    match regex::Regex::new(matcher) {
        Ok(re) => re.is_match(tool),
        Err(_) => {
            tracing::warn!(matcher = %matcher, "hook matcher is not valid regex");
            false
        }
    }
}

/// Whether the context satisfies a Tool(pattern) if-rule. The tool name
/// must match; a bare Tool (no parens) passes on tool match. A pattern is
/// glob-matched (star and question) against the tool input's string values
/// as an over-approximation. A non-tool event never passes.
pub(crate) fn if_rule_passes(ctx: &HookContext, rule: &str) -> bool {
    let (rule_tool, pattern) = parse_if_rule(rule);
    let Some(tool) = tool_name(ctx) else {
        return false;
    };
    if tool != rule_tool {
        return false;
    }
    let Some(pattern) = pattern else {
        return true;
    };
    let Some(input) = tool_input(ctx) else {
        return false;
    };
    glob_matches_any(&input, pattern)
}

/// Extract the tool name from a tool-lifecycle payload. Returns None for
/// non-tool events (the matcher and if-rule do not apply to them).
pub(crate) fn tool_name(ctx: &HookContext) -> Option<&str> {
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
    fn test_if_rule_with_pattern() {
        assert!(if_rule_passes(&ctx_pre("Bash"), "Bash(git *)"));
        assert!(!if_rule_passes(&ctx_pre("Bash"), "Bash(npm *)"));
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
}
