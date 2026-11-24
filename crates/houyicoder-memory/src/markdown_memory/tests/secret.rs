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
        "deploy with sk-ant-api03-abcdefghijklmnopqrst",
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
            .with_meta("token ghp_abcdefghijklmnopqrstuvwxyz", 0),
    );
    assert!(
        matches!(outcome, Err(MemoryError::Secret(_))),
        "{outcome:?}"
    );
    drop(remove_dir_all(&root));
}

/// Ordinary prose that happens to carry a prefix is not a credential: the
/// suffix length is what separates an identifier from a key.
#[test]
fn test_prefix_words_pass() {
    let root = root("words");
    let provider = MarkdownMemoryProvider::new(root.clone());
    for (key, body) in [
        ("region", "deploy to ASIA-EAST region"),
        ("registry", "npm_registry is configured"),
        ("hub", "hf_hub cache lives in the workspace"),
    ] {
        let outcome = provider.add(MemoryEntry::new(key, body, MemorySource::Reference));
        assert!(outcome.is_ok(), "{key}: {outcome:?}");
    }
    drop(remove_dir_all(&root));
}

/// An access key id has a fixed body length and alphabet, so a region name or
/// a short word after the prefix is not one.
#[test]
fn test_access_key_body() {
    let root = root("aws");
    let provider = MarkdownMemoryProvider::new(root.clone());
    let key = provider.add(MemoryEntry::new(
        "aws",
        "id AKIAABCDEFGHIJKLMNOP",
        MemorySource::Reference,
    ));
    assert!(matches!(key, Err(MemoryError::Secret(_))), "{key:?}");
    let word = provider.add(MemoryEntry::new(
        "region",
        "AKIA-EAST region",
        MemorySource::Reference,
    ));
    assert!(word.is_ok(), "{word:?}");
    drop(remove_dir_all(&root));
}
