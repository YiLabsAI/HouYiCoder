//! Loading the agent registry from the user and project directories.

use super::*;
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all, write};
use std::process::id;

/// A definition written to a directory loads and overrides the built-in of
/// the same type; a user definition yields to a project one.
#[test]
fn test_registry_layers_sources() {
    let tmp = temp_dir().join(format!("agent-load-{}", id()));
    drop(remove_dir_all(&tmp));
    let home = tmp.join("home");
    let project = tmp.join("project");
    for dir in [
        home.join(".houyicoder/agents"),
        home.join(".claude/agents"),
        project.join(".houyicoder/agents"),
    ] {
        create_dir_all(&dir).unwrap();
    }
    // A user definition: a new type the built-ins do not hold.
    write(
        home.join(".houyicoder/agents/reviewer.md"),
        "---\nname: reviewer\ndescription: reviews diffs\n---\nYou review diffs.",
    )
    .unwrap();
    // A project definition that overrides the built-in explore.
    write(
        project.join(".houyicoder/agents/explore.md"),
        "---\nname: explore\ndescription: project override\n---\nProject explore body.",
    )
    .unwrap();
    let registry = load_agent_registry(Some(&project), Some(&home));
    let listed = registry.list();
    assert!(
        listed.iter().any(|d| d.subagent_type == "reviewer"),
        "the user-level definition loads"
    );
    let explore = listed
        .iter()
        .find(|d| d.subagent_type == "explore")
        .expect("explore is present");
    assert_eq!(
        explore.when_to_use, "project override",
        "the project layer wins over the built-in"
    );
    // A missing home and workspace scan as empty: only the built-ins load.
    let bare = load_agent_registry(None, None);
    assert!(
        bare.list().iter().all(|d| d.subagent_type != "reviewer"),
        "no directories means no custom definitions"
    );
    drop(remove_dir_all(&tmp));
}

/// A corrupt definition file is skipped without blanking its layer: the
/// built-ins and the valid files beside it still load.
#[test]
fn test_registry_skips_corrupt() {
    let tmp = temp_dir().join(format!("agent-corrupt-{}", id()));
    drop(remove_dir_all(&tmp));
    let home = tmp.join("home");
    let dir = home.join(".houyicoder/agents");
    create_dir_all(&dir).unwrap();
    write(dir.join("broken.md"), "no frontmatter at all").unwrap();
    write(
        dir.join("fine.md"),
        "---\nname: fine\ndescription: a valid one\n---\nBody.",
    )
    .unwrap();
    let registry = load_agent_registry(None, Some(&home));
    let listed = registry.list();
    assert!(
        listed.iter().any(|d| d.subagent_type == "fine"),
        "the valid file beside the corrupt one loads"
    );
    assert!(
        listed.iter().all(|d| d.subagent_type != "broken"),
        "the corrupt file is skipped"
    );
    drop(remove_dir_all(&tmp));
}
