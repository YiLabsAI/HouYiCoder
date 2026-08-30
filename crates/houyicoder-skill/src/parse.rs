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
    let paths = field_string_list(&frontmatter, "paths").unwrap_or_default();
    let shell = field_bool(&frontmatter, "shell").unwrap_or(false);

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
        "shell",
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
        argument_hint,
        version,
        model,
        effort,
        disable_model_invocation,
        user_invocable,
        context,
        paths,
        shell,
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

fn field_bool(map: &serde_yaml::Mapping, key: &str) -> Option<bool> {
    map.get(serde_yaml::Value::String(key.into()))
        .and_then(|v| v.as_bool())
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
        assert!(!def.shell);
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
}
