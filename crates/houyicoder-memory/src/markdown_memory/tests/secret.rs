//! The write boundary refuses an entry that looks like a credential, before any
//! file is created, and names the rule rather than the matched text.

use super::super::MarkdownMemoryProvider;
use houyicoder_api::memory::MemoryProvider;
use houyicoder_context::{MemoryEntry, MemoryError, MemorySource};
use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::path::PathBuf;
use std::process::id;

fn root(tag: &str) -> PathBuf {
    let root = temp_dir().join(format!("secret-{tag}-{}", id()));
    drop(remove_dir_all(&root));
    create_dir_all(&root).expect("mkdir");
    root
}

#[test]
fn test_credential_write_refused() {
    let root = root("refuse");
    let provider = MarkdownMemoryProvider::new(root.clone());
    let outcome = provider.add(MemoryEntry::new(
        "leak",
        "deploy with sk-ant-api03-abcdef",
        MemorySource::Reference,
    ));
    assert!(
        matches!(outcome, Err(MemoryError::Secret(_))),
        "the write is refused: {outcome:?}"
    );
    assert!(!root.join("leak.md").exists(), "no record lands");
    drop(remove_dir_all(&root));
}

#[test]
fn test_ordinary_write_passes() {
    let root = root("pass");
    let provider = MarkdownMemoryProvider::new(root.clone());
    let outcome = provider.add(MemoryEntry::new(
        "policy",
        "integration tests hit a real database",
        MemorySource::Feedback,
    ));
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(root.join("policy.md").exists(), "the record lands");
    drop(remove_dir_all(&root));
}

#[test]
fn test_private_key_refused() {
    let root = root("pem");
    let provider = MarkdownMemoryProvider::new(root.clone());
    let outcome = provider.add(MemoryEntry::new(
        "key",
        "-----BEGIN RSA PRIVATE KEY-----\nMIIE\n-----END RSA PRIVATE KEY-----",
        MemorySource::Reference,
    ));
    assert!(
        matches!(outcome, Err(MemoryError::Secret(_))),
        "{outcome:?}"
    );
    drop(remove_dir_all(&root));
}

#[test]
fn test_description_checked() {
    let root = root("desc");
    let provider = MarkdownMemoryProvider::new(root.clone());
    let outcome = provider.add(
        MemoryEntry::new("note", "harmless body", MemorySource::Reference)
            .with_meta("token ghp_abcdef", 0),
    );
    assert!(
        matches!(outcome, Err(MemoryError::Secret(_))),
        "{outcome:?}"
    );
    drop(remove_dir_all(&root));
}
