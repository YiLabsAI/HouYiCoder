//! Skill invocation: argument substitution, variable substitution,
//! and body loading. These are pure data-processing functions with
//! no engine type dependencies (the actual Skill tool, which implements
//! the Tool trait, lives in the engine layer and calls these functions).

use std::path::Path;

use super::definition::SkillDefinition;
use super::disclose::resources;
use super::parse::split_frontmatter;
use std::fs::read_to_string;
use std::io::Error;
use std::mem::take;

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
pub fn load_skill_body(def: &SkillDefinition) -> Result<String, Error> {
    let raw = read_to_string(&def.body_path)?;
    let body = split_frontmatter(&raw)
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
/// - $ARGUMENTS[N] — replaced with the Nth parsed arg (0-indexed).
/// - $N — shorthand for $ARGUMENTS[N]; scanned high-to-low so $12 is
///   tried before $1; word-boundary guard prevents $1 from matching
///   inside $12 or $12abc.
///
/// An empty args string substitutes (replaces $ARGUMENTS with ""), unlike
/// None which leaves the body untouched — "no args provided" is distinct
/// from "empty args". No-placeholder fallback fires only when args were
/// actually provided (non-empty), so an empty-args call appends nothing.
pub fn substitute_args(body: &str, args: &str) -> String {
    let parts = parse_args(args);
    let mut consumed_args = false;

    let mut result = body.to_string();
    for n in (0..=99).rev() {
        let token = format!("${n}");
        let replacement = parts.get(n).map(|s| s.as_str()).unwrap_or("");
        let count = replace_token_with_boundary(&mut result, &token, replacement);
        if count > 0 {
            consumed_args = true;
        }
    }

    for n in (0..=99).rev() {
        let token = format!("$ARGUMENTS[{n}]");
        let replacement = parts.get(n).map(|s| s.as_str()).unwrap_or("");
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

    if !consumed_args && !args.is_empty() {
        result.push_str("\n\nARGUMENTS: ");
        result.push_str(args);
    }

    result
}

/// Parse an args string into tokens, honoring single + double quotes and
/// backslash escapes outside single quotes. A minimal shell-aware split so
/// the args string foo "hello world" baz yields three tokens (foo, hello
/// world, baz); the quote characters are consumed, not part of the token.
/// Matches the ecosystem standard shell-quote parser for common cases.
fn parse_args(args: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    for ch in args.chars() {
        if escape {
            cur.push(ch);
            escape = false;
        } else if ch == '\\' && !in_single {
            escape = true;
        } else if ch == '\'' && !in_double {
            in_single = !in_single;
        } else if ch == '"' && !in_single {
            in_double = !in_double;
        } else if ch.is_whitespace() && !in_single && !in_double {
            if !cur.is_empty() {
                parts.push(take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
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
/// then substitute variables. For sources allowed to surface a resource
/// manifest, the files alongside the body are listed by name + path
/// (content not loaded) so the model can address them without a
/// discovery call. Returns the final text ready for injection into the
/// agent message stream.
pub fn prepare_body(
    def: &SkillDefinition,
    args: Option<&str>,
    ctx: &SubstitutionContext,
) -> Result<String, Error> {
    let body = load_skill_body(def)?;
    let body = if let Some(a) = args {
        substitute_args(&body, a)
    } else {
        body
    };
    let mut body = substitute_variables(&body, ctx);
    // The manifest is appended after substitution so file paths are not
    // altered by arg/variable tokens. A missing or unreadable directory
    // yields an empty manifest, so a resource failure never breaks the body.
    if resources::is_manifest_eligible(&def.source) {
        let manifest = resources::format_manifest(&resources::list_resources(&def.skill_dir));
        body.push_str(&manifest);
    }
    Ok(body)
}

/// Replace a token with a word-boundary guard. Returns the number of
/// replacements made. The guard ensures $N is not followed by a word
/// char (digit/letter/underscore), so $1 does not match inside $12 or
/// $12abc, matching the ecosystem standard's (?!\w) boundary.
fn replace_token_with_boundary(text: &mut String, token: &str, replacement: &str) -> usize {
    let token_bytes = token.as_bytes();
    let token_len = token.len();

    let mut start = 0;
    let mut positions = Vec::new();
    let bytes = text.as_bytes();
    while start + token_len <= bytes.len() {
        if &bytes[start..start + token_len] == token_bytes {
            let next = bytes.get(start + token_len).copied();
            let is_followed_by_word = next
                .map(|b| b.is_ascii_alphanumeric() || b == b'_')
                .unwrap_or(false);
            if !is_followed_by_word {
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
mod tests;
