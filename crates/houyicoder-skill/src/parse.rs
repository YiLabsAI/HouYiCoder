//! Frontmatter parser: reads SKILL.md, splits frontmatter from body,
//! and parses into SkillDefinition using serde_yaml with tolerant
//! fallback (aligned with grok-build discovery.rs pattern: raw map
//! + per-field coercion + line-by-line recovery).

use std::path::Path;

use super::definition::{SkillContext, SkillDefinition, SkillSource, SpecFields};

/// Split a SKILL.md file into (yaml_frontmatter_str, body_str).
pub fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let trimmed = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let end = trimmed
        .find("\n---\n")
        .or_else(|| trimmed.find("\n---\r\n"))
        .or_else(|| trimmed.find("\r\n---\r\n"))?;
    let yaml = &trimmed[..end];
    let body_start = end + "\n---\n".len();
    let body = &trimmed[body_start..];
    Some((yaml, body))
}

/// Parse a SKILL.md file into a SkillDefinition. Tolerant: one bad
/// field does not drop its siblings. Unknown fields are passed through.
#[allow(clippy::too_many_lines)]
pub fn parse_skill(
    text: &str,
    dir_name: &str,
    skill_dir: &Path,
    body_path: &Path,
    source: SkillSource,
) -> Result<SkillDefinition, ParseError> {
    let (yaml_str, body) = split_frontmatter(text).unwrap_or(("", text));

    let frontmatter: serde_yaml::Mapping = serde_yaml::from_str(yaml_str).unwrap_or_else(|err| {
        tracing::debug!(error = %err, "frontmatter YAML parse failed; recovering scalars");
        recover_scalar_fields(yaml_str)
    });

    let display_name = field_string(&frontmatter, "name");
    // The directory name is the skill identity (matches the ecosystem
    // standard: the directory is the slash-command name + the dedup key; the
    // frontmatter name is display-only). The frontmatter name never overrides
    // the identity, so a skill in a directory named "commit" is invoked as
    // /commit regardless of its frontmatter name.
    let name = dir_name.to_string();

    let description = field_string(&frontmatter, "description")
        .or_else(|| {
            body.lines()
                .find(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with('#')
                })
                .map(str::to_string)
        })
        .ok_or(ParseError::MissingDescription)?;

    let when_to_use = field_string(&frontmatter, "when_to_use")
        .or_else(|| field_string(&frontmatter, "whenToUse"));
    let allowed_tools = field_string_list(&frontmatter, "allowed-tools")
        .or_else(|| field_string_list(&frontmatter, "allowed_tools"))
        .unwrap_or_default();
    let allowed_mach_services = field_string_list(&frontmatter, "allowed-mach-services")
        .or_else(|| field_string_list(&frontmatter, "allowed_mach_services"))
        .unwrap_or_default();
    let argument_hint = field_string(&frontmatter, "argument-hint")
        .or_else(|| field_string(&frontmatter, "argumentHint"));
    let version = field_string(&frontmatter, "version");
    let model = field_string(&frontmatter, "model");
    let effort = field_string(&frontmatter, "effort");
    let disable_model_invocation = field_bool(&frontmatter, "disable-model-invocation")
        .or_else(|| field_bool(&frontmatter, "disableModelInvocation"))
        .unwrap_or(false);
    let user_invocable = field_bool(&frontmatter, "user-invocable")
        .or_else(|| field_bool(&frontmatter, "userInvocable"))
        .unwrap_or(true);
    let paths = parse_skill_paths(&frontmatter);

    let context_str = field_string(&frontmatter, "context");
    let context = match context_str.as_deref() {
        Some("fork") => {
            let agent = field_string(&frontmatter, "agent").unwrap_or_default();
            SkillContext::Fork(agent)
        }
        _ => SkillContext::Inline,
    };

    let hooks_raw = frontmatter
        .get(serde_yaml::Value::String("hooks".into()))
        .cloned()
        .filter(|v| !v.is_null());

    let spec_fields = SpecFields {
        license: field_string(&frontmatter, "license"),
        compatibility: field_string(&frontmatter, "compatibility"),
        metadata: frontmatter
            .get(serde_yaml::Value::String("metadata".into()))
            .and_then(|v| v.as_mapping())
            .cloned()
            .unwrap_or_default(),
    };

    let known_keys: &[&str] = &[
        "name",
        "description",
        "when_to_use",
        "whenToUse",
        "allowed-tools",
        "allowed_tools",
        "allowed-mach-services",
        "allowed_mach_services",
        "argument-hint",
        "argumentHint",
        "version",
        "model",
        "effort",
        "disable-model-invocation",
        "disableModelInvocation",
        "user-invocable",
        "userInvocable",
        "hooks",
        "context",
        "agent",
        "paths",
        "license",
        "compatibility",
        "metadata",
    ];
    let unknown_fields: serde_yaml::Mapping = frontmatter
        .iter()
        .filter(|(k, _)| k.as_str().map(|s| !known_keys.contains(&s)).unwrap_or(true))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    if let Some(ref dn) = display_name
        && dn != dir_name
    {
        tracing::warn!(skill = %dir_name, frontmatter_name = %dn, "name mismatch; directory name is identity");
    }

    if !is_valid_skill_name(&name) {
        tracing::warn!(skill = %name, "name does not match spec; loaded anyway (lenient)");
    }

    Ok(SkillDefinition {
        name,
        display_name,
        description,
        when_to_use,
        allowed_tools,
        allowed_mach_services,
        argument_hint,
        version,
        model,
        effort,
        disable_model_invocation,
        user_invocable,
        context,
        paths,
        source,
        body_path: body_path.to_path_buf(),
        skill_dir: skill_dir.to_path_buf(),
        hooks_raw,
        spec_fields,
        unknown_fields,
    })
}

fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.contains("--")
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("missing description and no body fallback")]
    MissingDescription,
}

fn field_string(map: &serde_yaml::Mapping, key: &str) -> Option<String> {
    map.get(serde_yaml::Value::String(key.into()))
        .and_then(coerce_to_string)
}

/// Parse a frontmatter bool, accepting both YAML bool and string "true"/"false".
/// The string form matters when the YAML fallback stores every field as a
/// string; without it, bool fields like disable-model-invocation would be
/// lost (fail-open) on a malformed-YAML recovery.
fn field_bool(map: &serde_yaml::Mapping, key: &str) -> Option<bool> {
    map.get(serde_yaml::Value::String(key.into()))
        .and_then(|v| match v {
            serde_yaml::Value::Bool(b) => Some(*b),
            serde_yaml::Value::String(s) => match s.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            },
            _ => None,
        })
}

fn field_string_list(map: &serde_yaml::Mapping, key: &str) -> Option<Vec<String>> {
    map.get(serde_yaml::Value::String(key.into()))
        .and_then(|v| {
            if let Some(seq) = v.as_sequence() {
                Some(seq.iter().filter_map(coerce_to_string).collect())
            } else {
                coerce_to_string(v).map(|s| s.split_whitespace().map(str::to_string).collect())
            }
        })
}

fn coerce_to_string(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Normalize the paths frontmatter into gitignore-style glob patterns.
/// The raw value is a comma-separated string or a YAML list; brace
/// expansion turns src/*.{ts,tsx} into two patterns. A trailing /** is
/// stripped because the gitignore matcher treats a bare directory name as
/// matching itself and all descendants. If every pattern reduces to **
/// (match-all) or the list is empty, the skill is unconditional (empty =
/// always visible). A malformed value silently yields an empty vec —
/// fail-open to visible. This direction is intentional: a skill that
/// narrows its own visibility with a broken pattern widening to
/// always-visible is the safe default since skill visibility defaults to
/// on. C1 pins this with a test so a future change cannot silently flip
/// it to fail-closed.
fn parse_skill_paths(map: &serde_yaml::Mapping) -> Vec<String> {
    let Some(value) = map.get(serde_yaml::Value::String("paths".into())) else {
        return Vec::new();
    };
    let mut patterns: Vec<String> = Vec::new();
    for raw in collect_paths_strings(value) {
        for part in split_comma_brace_aware(&raw) {
            if part.is_empty() {
                continue;
            }
            patterns.extend(expand_braces(&part));
        }
    }
    for p in &mut patterns {
        if p.ends_with("/**") {
            p.truncate(p.len() - "/**".len());
        }
    }
    patterns.retain(|p| !p.is_empty());
    if patterns.iter().all(|p| p == "**") {
        return Vec::new();
    }
    patterns
}

/// Collect the raw path strings from a paths value, recursing into a YAML
/// sequence so a nested list flattens the way an array flatMap does. A
/// scalar yields a single string.
fn collect_paths_strings(value: &serde_yaml::Value) -> Vec<String> {
    match value {
        serde_yaml::Value::Sequence(seq) => seq.iter().flat_map(collect_paths_strings).collect(),
        v => coerce_to_string(v).into_iter().collect(),
    }
}

/// Split a string on commas while respecting brace depth, so {a,b},c keeps
/// the brace group intact. Each part is trimmed; empty parts are dropped.
fn split_comma_brace_aware(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    for ch in s.chars() {
        match ch {
            '{' => {
                depth += 1;
                current.push(ch);
            }
            '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    parts.push(trimmed);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        parts.push(trimmed);
    }
    parts
}

/// Expand brace groups into one pattern per combination. src/*.{ts,tsx}
/// becomes two patterns; {a,b}/{c,d} becomes the four crosses. Nested and
/// multiple groups expand left-to-right via recursion. An unmatched brace
/// (no closing brace) is treated as a literal character.
fn expand_braces(pattern: &str) -> Vec<String> {
    let chars: Vec<char> = pattern.chars().collect();
    expand_braces_chars(&chars)
}

fn expand_braces_chars(chars: &[char]) -> Vec<String> {
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '{' {
            let mut depth = 1;
            let mut j = i + 1;
            while j < chars.len() && depth > 0 {
                match chars[j] {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    break;
                }
                j += 1;
            }
            if depth == 0 {
                let prefix: String = chars[..i].iter().collect();
                let group: String = chars[i + 1..j].iter().collect();
                let suffix: &[char] = &chars[j + 1..];
                let options = split_comma_brace_aware(&group);
                let mut results = Vec::new();
                for opt in options {
                    let mut combined: Vec<char> = prefix.chars().collect();
                    combined.extend(opt.chars());
                    combined.extend(suffix.iter().copied());
                    results.extend(expand_braces_chars(&combined));
                }
                return results;
            }
        }
        i += 1;
    }
    vec![chars.iter().collect()]
}

fn recover_scalar_fields(yaml_str: &str) -> serde_yaml::Mapping {
    let mut map = serde_yaml::Mapping::new();
    for line in yaml_str.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if !k.is_empty() && !v.is_empty() {
                map.insert(
                    serde_yaml::Value::String(k.to_string()),
                    serde_yaml::Value::String(v.to_string()),
                );
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_skill() {
        let text = "---\nname: my-skill\ndescription: Does a thing\n---\nbody text";
        let def = parse_skill(
            text,
            "my-skill",
            Path::new("/tmp/my-skill"),
            Path::new("/tmp/my-skill/SKILL.md"),
            SkillSource::User,
        )
        .expect("parse");
        assert_eq!(def.name, "my-skill");
        assert_eq!(def.description, "Does a thing");
        assert!(def.user_invocable);
    }

    #[test]
    fn test_description_from_body() {
        let text = "---\nname: no-desc\n---\nThis is the first line of body.";
        let def = parse_skill(
            text,
            "no-desc",
            Path::new("/tmp/no-desc"),
            Path::new("/tmp/no-desc/SKILL.md"),
            SkillSource::Project,
        )
        .expect("parse");
        assert_eq!(def.description, "This is the first line of body.");
    }

    #[test]
    fn test_missing_description_skips() {
        let text = "---\nname: empty\n---\n";
        assert!(
            parse_skill(
                text,
                "empty",
                Path::new("/tmp/empty"),
                Path::new("/tmp/empty/SKILL.md"),
                SkillSource::Project
            )
            .is_err()
        );
    }

    #[test]
    fn test_unknown_fields_pass_through() {
        let text = "---\nname: test\ndescription: test\ncustom_field: value\n---\nbody";
        let def = parse_skill(
            text,
            "test",
            Path::new("/tmp/test"),
            Path::new("/tmp/test/SKILL.md"),
            SkillSource::User,
        )
        .expect("parse");
        assert!(
            def.unknown_fields
                .contains_key(serde_yaml::Value::String("custom_field".into()))
        );
    }

    #[test]
    fn test_name_mismatch_warns() {
        let text = "---\nname: DifferentName\ndescription: test\n---\nbody";
        let def = parse_skill(
            text,
            "dir-name",
            Path::new("/tmp/dir-name"),
            Path::new("/tmp/dir-name/SKILL.md"),
            SkillSource::User,
        )
        .expect("parse");
        // Directory name is identity; frontmatter name is display-only.
        assert_eq!(def.name, "dir-name", "directory name is the identity");
        assert_eq!(
            def.display_name.as_deref(),
            Some("DifferentName"),
            "frontmatter name is display-only"
        );
    }

    #[test]
    fn test_fork_context() {
        let text =
            "---\nname: forked\ndescription: test\ncontext: fork\nagent: reviewer\n---\nbody";
        let def = parse_skill(
            text,
            "forked",
            Path::new("/tmp/forked"),
            Path::new("/tmp/forked/SKILL.md"),
            SkillSource::User,
        )
        .expect("parse");
        assert_eq!(def.context, SkillContext::Fork("reviewer".into()));
    }

    #[test]
    fn test_bad_yaml_recovers() {
        let text = "---\nname: broken\ndescription: test\nbad: value: with: colons\n---\nbody";
        assert!(
            parse_skill(
                text,
                "broken",
                Path::new("/tmp/broken"),
                Path::new("/tmp/broken/SKILL.md"),
                SkillSource::User
            )
            .is_ok()
        );
    }

    #[test]
    fn test_name_validation_warns() {
        let text = "---\nname: Bad_Name\ndescription: test\n---\nbody";
        assert!(
            parse_skill(
                text,
                "Bad_Name",
                Path::new("/tmp/Bad_Name"),
                Path::new("/tmp/Bad_Name/SKILL.md"),
                SkillSource::User
            )
            .is_ok()
        );
    }

    /// field_bool accepts both YAML bool and string "true"/"false", so the
    /// YAML-fallback recovery path (which stores everything as String) does
    /// not lose bool fields. Without this, disable-model-invocation would
    /// fail-open (false) on a malformed-YAML recovery.
    #[test]
    fn test_field_bool_string_form() {
        let mut map = serde_yaml::Mapping::new();
        map.insert(
            serde_yaml::Value::String("k".into()),
            serde_yaml::Value::String("true".into()),
        );
        assert_eq!(field_bool(&map, "k"), Some(true), "String(true) -> true");

        map.insert(
            serde_yaml::Value::String("k".into()),
            serde_yaml::Value::String("false".into()),
        );
        assert_eq!(field_bool(&map, "k"), Some(false), "String(false) -> false");

        map.insert(
            serde_yaml::Value::String("k".into()),
            serde_yaml::Value::Bool(true),
        );
        assert_eq!(field_bool(&map, "k"), Some(true), "Bool(true) -> true");
    }

    /// A malformed-YAML recovery preserves disable-model-invocation. The
    /// tab in indentation breaks serde_yaml; the scalar recovery stores
    /// "true" as a string; field_bool must read it. Under the old code this
    /// returned false (fail-open: the model could invoke a disabled skill).
    #[test]
    fn test_malformed_yaml_preserves_disable() {
        let text =
            "---\n\tdisable-model-invocation: true\ndescription: test\nname: test\n---\nbody";
        let def = parse_skill(
            text,
            "test",
            Path::new("/tmp/test"),
            Path::new("/tmp/test/SKILL.md"),
            SkillSource::User,
        )
        .expect("parse with recovery");
        assert!(
            def.disable_model_invocation,
            "disable-model-invocation preserved through YAML recovery"
        );
    }

    fn paths_map(value: serde_yaml::Value) -> serde_yaml::Mapping {
        let mut m = serde_yaml::Mapping::new();
        m.insert(serde_yaml::Value::String("paths".into()), value);
        m
    }

    #[test]
    fn test_paths_comma_split() {
        let map = paths_map(serde_yaml::Value::String("src, docs".into()));
        assert_eq!(
            parse_skill_paths(&map),
            vec!["src".to_string(), "docs".to_string()]
        );
    }

    #[test]
    fn test_paths_yaml_list() {
        let seq = serde_yaml::Value::Sequence(vec![
            serde_yaml::Value::String("src".into()),
            serde_yaml::Value::String("docs".into()),
        ]);
        let map = paths_map(seq);
        assert_eq!(
            parse_skill_paths(&map),
            vec!["src".to_string(), "docs".to_string()]
        );
    }

    #[test]
    fn test_paths_brace_single() {
        let map = paths_map(serde_yaml::Value::String("src/*.{ts,tsx}".into()));
        assert_eq!(
            parse_skill_paths(&map),
            vec!["src/*.ts".to_string(), "src/*.tsx".to_string()]
        );
    }

    #[test]
    fn test_paths_brace_cross() {
        // {a,b}/{c,d} expands to the four crosses, left-to-right.
        let map = paths_map(serde_yaml::Value::String("{a,b}/{c,d}".into()));
        assert_eq!(
            parse_skill_paths(&map),
            vec![
                "a/c".to_string(),
                "a/d".to_string(),
                "b/c".to_string(),
                "b/d".to_string(),
            ]
        );
    }

    #[test]
    fn test_paths_strip_suffix() {
        // A bare directory name matches itself + descendants under
        // gitignore semantics, so the trailing /** is stripped at parse.
        let map = paths_map(serde_yaml::Value::String("src/**".into()));
        assert_eq!(parse_skill_paths(&map), vec!["src".to_string()]);
    }

    #[test]
    fn test_paths_allstar_unconditional() {
        // A lone ** matches everything, so the skill is unconditional
        // (empty = always visible).
        let map = paths_map(serde_yaml::Value::String("**".into()));
        assert!(parse_skill_paths(&map).is_empty());
        let seq = serde_yaml::Value::Sequence(vec![
            serde_yaml::Value::String("**".into()),
            serde_yaml::Value::String("**".into()),
        ]);
        let map = paths_map(seq);
        assert!(parse_skill_paths(&map).is_empty());
    }

    #[test]
    fn test_paths_missing_unconditional() {
        let map = serde_yaml::Mapping::new();
        assert!(parse_skill_paths(&map).is_empty());
    }

    /// A malformed paths value (a mapping where a string or list is
    /// expected) fails open to empty — the skill becomes unconditional
    /// (always visible). This is intentional: skill visibility defaults to
    /// on, and a broken pattern widening to always-visible is the safe
    /// direction. Pinning it here stops a future change from silently
    /// flipping to fail-closed.
    #[test]
    fn test_paths_malformed_open() {
        let nested = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let map = paths_map(nested);
        assert!(
            parse_skill_paths(&map).is_empty(),
            "malformed paths value must fail open to unconditional"
        );
    }

    /// An unmatched brace (no closing brace) is treated as a literal
    /// character — no expansion, the pattern survives as-is. Pins the
    /// documented behavior so a refactor cannot silently change it.
    #[test]
    fn test_paths_unmatched_brace() {
        let map = paths_map(serde_yaml::Value::String("foo{bar".into()));
        assert_eq!(parse_skill_paths(&map), vec!["foo{bar".to_string()]);
    }
}
