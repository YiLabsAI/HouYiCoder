//! Tests for this module.

use std::fs;
use std::path::PathBuf;

use super::{DefinitionSource, LoadError, Loader, load_dir, parse_agent_definition};
use crate::agent::multi_agent::registry::PromptSource;

#[test]
fn test_parse_minimal_definition() {
    let text =
        "---\nname: researcher\ndescription: reads sources and summarizes\n---\nDo the research.";
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
    let on = parse_agent_definition("---\nname: scanner\nomit_project_context: true\n---\nbody")
        .unwrap();
    assert!(on.omit_project_context);

    let off = parse_agent_definition("---\nname: other\n---\nbody").unwrap();
    assert!(!off.omit_project_context, "absent key defaults to false");

    let camel =
        parse_agent_definition("---\nname: camel\nomitProjectContext: true\n---\nbody").unwrap();
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
        parse_agent_definition("---\nname: shared\ndescription: project version\n---\np").unwrap(),
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
