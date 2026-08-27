//! Skill invocation: argument substitution, variable substitution,
//! and body loading. These are pure data-processing functions with
//! no engine type dependencies (the actual Skill tool, which implements
//! the Tool trait, lives in the engine layer and calls these functions).

use std::path::Path;

use super::definition::SkillDefinition;

/// Context for variable substitution. All fields are optional; a
/// missing field means the corresponding variable is left as-is.
#[derive(Debug, Clone, Default)]
pub struct SubstitutionContext<'a> {
    pub skill_dir: Option<&'a Path>,
    pub session_id: Option<&'a str>,
    pub plugin_root: Option<&'a Path>,
}

/// Load the skill body text from disk, strip frontmatter, and prepend
/// the base-dir header. Returns the prepared body text ready for
/// args + variable substitution.
pub fn load_skill_body(def: &SkillDefinition) -> Result<String, std::io::Error> {
    let raw = std::fs::read_to_string(&def.body_path)?;
    let body = super::parse::split_frontmatter(&raw)
        .map(|(_, body)| body)
        .unwrap_or(&raw);
    let header = format!(
        "Base directory for this skill: {}\n\n",
        def.skill_dir.display()
    );
    Ok(format!("{header}{body}"))
}

/// Substitute arguments ($ARGUMENTS, $ARGUMENTS[N], $N) and apply the
/// no-placeholder fallback in the body text.
///
/// Token reference (aligned with the ecosystem standard):
/// - $ARGUMENTS — replaced with the full arguments string.
/// - $ARGUMENTS[N] — replaced with the Nth whitespace-split arg (0-indexed).
/// - $N — shorthand for $ARGUMENTS[N]; scanned high-to-low so $12 is
///   tried before $1; word-boundary guard prevents $100 from matching
///   $1 + literal "00".
///
/// No-placeholder fallback: if no argument token consumed the args,
/// append "ARGUMENTS: {args}" to the end of the body so user input
/// is never silently lost. Path/variable tokens ($SKILL_DIR etc.)
/// do NOT suppress the fallback — only true argument tokens do.
pub fn substitute_args(body: &str, args: &str) -> String {
    if args.is_empty() {
        return body.to_string();
    }

    let parts: Vec<&str> = args.split_whitespace().collect();
    let mut consumed_args = false;

    let mut result = body.to_string();
    for n in (0..=99).rev() {
        let token = format!("${n}");
        let replacement = parts.get(n).copied().unwrap_or("");
        let count = replace_token_with_boundary(&mut result, &token, replacement);
        if count > 0 {
            consumed_args = true;
        }
    }

    for n in (0..=99).rev() {
        let token = format!("$ARGUMENTS[{n}]");
        let replacement = parts.get(n).copied().unwrap_or("");
        let before = result.clone();
        result = result.replace(&token, replacement);
        if result != before {
            consumed_args = true;
        }
    }

    if result.contains("$ARGUMENTS") {
        result = result.replace("$ARGUMENTS", args);
        consumed_args = true;
    }

    if !consumed_args {
        result.push_str("\n\n**ARGUMENTS:** ");
        result.push_str(args);
    }

    result
}

/// Substitute environment variables in the body text. Dual aliases
/// ensure ecosystem compatibility (skills written with one naming
/// convention work with the other).
///
/// Aliases:
/// - ${HOUYI_SKILL_DIR} / ${CLAUDE_SKILL_DIR} — skill directory path
/// - ${HOUYI_SESSION_ID} / ${CLAUDE_SESSION_ID} — current session id
/// - ${CLAUDE_PLUGIN_ROOT} — plugin root (for hooks)
pub fn substitute_variables(body: &str, ctx: &SubstitutionContext) -> String {
    let mut result = body.to_string();

    if let Some(dir) = ctx.skill_dir {
        let dir_str = dir.to_string_lossy();
        result = result.replace("${HOUYI_SKILL_DIR}", &dir_str);
        result = result.replace("${CLAUDE_SKILL_DIR}", &dir_str);
    }
    if let Some(sid) = ctx.session_id {
        result = result.replace("${HOUYI_SESSION_ID}", sid);
        result = result.replace("${CLAUDE_SESSION_ID}", sid);
    }
    if let Some(root) = ctx.plugin_root {
        let root_str = root.to_string_lossy();
        result = result.replace("${CLAUDE_PLUGIN_ROOT}", &root_str);
        result = result.replace("${HOUYI_PLUGIN_ROOT}", &root_str);
    }

    result
}

/// Full preparation pipeline: load body from disk, substitute args,
/// then substitute variables. Returns the final text ready for
/// injection into the agent message stream.
pub fn prepare_body(
    def: &SkillDefinition,
    args: Option<&str>,
    ctx: &SubstitutionContext,
) -> Result<String, std::io::Error> {
    let body = load_skill_body(def)?;
    let body = if let Some(a) = args {
        substitute_args(&body, a)
    } else {
        body
    };
    Ok(substitute_variables(&body, ctx))
}

/// Replace a token with a word-boundary guard. Returns the number of
/// replacements made. The guard ensures $N is not followed by another
/// digit (which would make $1 match the "1" in $12).
fn replace_token_with_boundary(text: &mut String, token: &str, replacement: &str) -> usize {
    let token_bytes = token.as_bytes();
    let token_len = token.len();

    let mut start = 0;
    let mut positions = Vec::new();
    let bytes = text.as_bytes();
    while start + token_len <= bytes.len() {
        if &bytes[start..start + token_len] == token_bytes {
            let next = bytes.get(start + token_len).copied();
            let is_followed_by_digit = next.map(|b| b.is_ascii_digit()).unwrap_or(false);
            if !is_followed_by_digit {
                positions.push((start, token_len));
            }
            start += token_len;
        } else {
            start += 1;
        }
    }

    let count = positions.len();
    for (pos, len) in positions.into_iter().rev() {
        let end = pos + len;
        text.replace_range(pos..end, replacement);
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arguments_full() {
        let body = "echo $ARGUMENTS";
        let result = substitute_args(body, "hello world");
        assert_eq!(result, "echo hello world");
    }

    #[test]
    fn test_arguments_indexed() {
        let body = "first: $0, second: $1";
        let result = substitute_args(body, "foo bar baz");
        assert_eq!(result, "first: foo, second: bar");
    }

    #[test]
    fn test_shorthand_high_to_low() {
        // $12 must not be split into $1 + "2". 0-indexed: parts[12] = "m".
        let body = "val: $12";
        let result = substitute_args(body, "a b c d e f g h i j k l m");
        assert_eq!(result, "val: m");
    }

    #[test]
    fn test_explicit_index() {
        let body = "val: $ARGUMENTS[2]";
        let result = substitute_args(body, "a b c");
        assert_eq!(result, "val: c");
    }

    #[test]
    fn test_no_placeholder_fallback() {
        let body = "just some text";
        let result = substitute_args(body, "extra input");
        assert!(
            result.contains("**ARGUMENTS:** extra input"),
            "must append fallback"
        );
    }

    #[test]
    fn test_placeholder_suppresses_fallback() {
        let body = "args: $ARGUMENTS";
        let result = substitute_args(body, "hello");
        assert!(
            !result.contains("**ARGUMENTS:** "),
            "must not append fallback"
        );
        assert_eq!(result, "args: hello");
    }

    #[test]
    fn test_empty_args_unchanged() {
        let body = "no tokens here";
        let result = substitute_args(body, "");
        assert_eq!(result, body);
    }

    #[test]
    fn test_variable_skill_dir() {
        let body = "dir is ${HOUYI_SKILL_DIR}";
        let ctx = SubstitutionContext {
            skill_dir: Some(Path::new("/home/user/skills/my-skill")),
            ..Default::default()
        };
        let result = substitute_variables(body, &ctx);
        assert_eq!(result, "dir is /home/user/skills/my-skill");
    }

    #[test]
    fn test_variable_claude_alias() {
        let body = "dir is ${CLAUDE_SKILL_DIR}";
        let ctx = SubstitutionContext {
            skill_dir: Some(Path::new("/home/user/skills/my-skill")),
            ..Default::default()
        };
        let result = substitute_variables(body, &ctx);
        assert_eq!(result, "dir is /home/user/skills/my-skill");
    }

    #[test]
    fn test_variable_session_id() {
        let body = "sid: ${HOUYI_SESSION_ID}";
        let ctx = SubstitutionContext {
            session_id: Some("abc-123"),
            ..Default::default()
        };
        let result = substitute_variables(body, &ctx);
        assert_eq!(result, "sid: abc-123");
    }

    #[test]
    fn test_variable_missing_left_asis() {
        let body = "dir: ${HOUYI_SKILL_DIR}";
        let ctx = SubstitutionContext::default();
        let result = substitute_variables(body, &ctx);
        assert_eq!(result, body);
    }

    #[test]
    fn test_boundary_guard() {
        // $1 must not partially match inside $10. Both should match
        // independently: $10 → parts[10], $1 → parts[1].
        let body = "$1 and $10";
        let result = substitute_args(body, "a b c d e f g h i j k");
        // $10 = parts[10] = "k", $1 = parts[1] = "b". Boundary guard
        // prevents $1 from matching the "1" inside "$10".
        assert_eq!(result, "b and k");
    }
}
