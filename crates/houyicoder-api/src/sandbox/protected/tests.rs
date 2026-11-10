//! Tests for the protected path judgement.

use crate::sandbox::credentials::CREDENTIAL_FILES;

use super::{fence_covers_all_hits, fence_denied_path, protected_path_hits};

/// The startup file list and the credential list must stay in step: a name
/// added to one and not the other opens a path the other list closes.
#[test]
fn test_startup_files_are_credentials() {
    for name in super::STARTUP_FILES {
        assert!(
            CREDENTIAL_FILES.contains(name),
            "{name} must also be recognized as a credential file"
        );
    }
}

/// A configuration store, the version-control directory, and a startup file
/// are each reported under their own family, because the gate escalates on a
/// mention of these and not on a read of an instruction file.
#[test]
fn test_families_reported_apart() {
    let control = protected_path_hits("echo x > .claude/settings.json");
    assert!(control.control && !control.startup && !control.instruction && !control.memory);
    let startup = protected_path_hits("cat ~/.zshrc");
    assert!(startup.startup && !startup.control && !startup.instruction && !startup.memory);
    let memory = protected_path_hits(".houyicoder/projects/p1/memory/notes.md");
    assert!(memory.memory && !memory.control);
    assert!(protected_path_hits("cat AGENTS.md").instruction);
}

/// A file whose name merely ends in a protected name is not that file, and a
/// backup beside a store decides nothing.
#[test]
fn test_similar_name_not_protected() {
    for text in [
        "./install.bashrc",
        "AGENTS.md.bak",
        ".claude/settings.json.bak",
        "/tmp/memory/notes.md",
    ] {
        assert!(!protected_path_hits(text).any(), "{text}");
    }
}

/// The fence denies writes to the settings store but not to the hooks
/// directory of a configuration directory, so a refusal may only stand where
/// the kernel refuses.
#[test]
fn test_fence_covers_only_denied() {
    assert!(fence_denied_path(".houyicoder/settings.json"));
    assert!(fence_covers_all_hits(".houyicoder/settings.json"));
    assert!(fence_denied_path("repo/.git/hooks/pre-commit"));
    assert!(fence_covers_all_hits("repo/.git/hooks/pre-commit"));
    assert!(fence_denied_path("HOME=/tmp/.claude/.credentials.json"));
    for text in [
        ".houyicoder/hooks/run.sh",
        ".git/config",
        ".git/refs/heads/main",
        "cat ~/.bashrc",
    ] {
        assert!(!fence_covers_all_hits(text), "{text}");
    }
}
