//! A rule set the gate could not read at startup. The gate may not fall back
//! to the rules it happens to hold: a deny rule in the unread file would be
//! lost. Every verdict short of a deny becomes a safety ask that consent
//! cannot override, and headless turns that ask into a refusal.

use std::env::temp_dir;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::id;
use std::sync::Arc;

use crate::consent::ConsentStore;
use crate::consent::InMemoryConsentStore;
use crate::decision::AskSource;
use crate::decision::Decision;
use crate::decision::DenySource;
use crate::mode::PermissionMode;
use crate::rule::Effect;
use crate::rule::Rule;
use crate::store::FileRuleStore;
use crate::store::RuleStore;
use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};

use super::*;

fn temp_root(tag: &str) -> PathBuf {
    let root = temp_dir().join(format!("houyi-gate-store-{tag}-{}", id()));
    remove_dir_all(&root).ok();
    create_dir_all(&root).expect("create the store root");
    root
}

fn corrupt_store(root: &Path) -> Arc<dyn RuleStore> {
    write(root.join("user.json"), b"{ this is not a permissions file")
        .expect("write the corrupt user file");
    Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ))
}

/// The user file names the store root that cannot be decoded, so the startup
/// notice built from this text points the user at the right file.
#[test]
fn test_unreadable_store_asks() {
    let root = temp_root("asks");
    let gate = DefaultModeGate::new_without_builtins().with_store(corrupt_store(&root));
    let failure = gate.store_failure().expect("the failure is recorded");
    assert!(
        failure.contains(&root.join("user.json").display().to_string()),
        "the recorded failure names the file: {failure}"
    );
    let decision = gate.decide(&req("read", false, true, false));
    let Decision::Ask(reason) = &decision else {
        panic!("a read the mode allows must still ask: {decision:?}");
    };
    assert_eq!(reason.source, AskSource::SystemSafety);
    assert!(
        reason.detail.contains("user.json"),
        "the ask must name the file to repair: {}",
        reason.detail
    );
    remove_dir_all(&root).ok();
}

/// Headless has no reader for the approval card, so the same call is refused
/// rather than left waiting on an answer that cannot arrive.
#[test]
fn test_headless_denies_unreadable() {
    let root = temp_root("headless");
    let gate = DefaultModeGate::new_without_builtins()
        .with_store(corrupt_store(&root))
        .with_headless(true);
    let decision = gate.decide(&req("read", false, true, false));
    let Decision::Deny(reason) = &decision else {
        panic!("headless has no one to ask: {decision:?}");
    };
    assert_eq!(reason.source, DenySource::Headless);
    remove_dir_all(&root).ok();
}

/// A deny rule already in memory was read before the store failed, so it
/// stays authoritative. Turning it into an ask would offer the user a yes the
/// deny rule never allowed.
#[test]
fn test_deny_rule_survives_failure() {
    let root = temp_root("deny");
    let gate = DefaultModeGate::new_without_builtins();
    gate.add_rule(Rule::new("read", Effect::Deny).expect("a deny rule"))
        .expect("the rule is kept in memory");
    let gate = gate.with_store(corrupt_store(&root));
    let decision = gate.decide(&req("read", false, true, false));
    let Decision::Deny(reason) = &decision else {
        panic!("a deny rule outranks the unread store: {decision:?}");
    };
    assert_eq!(reason.source, DenySource::UserRule);
    remove_dir_all(&root).ok();
}

/// A store whose files are simply absent holds no rules: that is an ordinary
/// first run, not a failure, and the mode keeps deciding on its own.
#[test]
fn test_missing_store_allows() {
    let root = temp_root("missing");
    let store = FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    );
    let gate = DefaultModeGate::new_without_builtins().with_store(Arc::new(store));
    assert!(
        gate.store_failure().is_none(),
        "an absent file is no failure"
    );
    assert!(matches!(
        gate.decide(&req("read", false, true, false)),
        Decision::Allow(_)
    ));
    remove_dir_all(&root).ok();
}

/// A stored consent is a yes for one exact call. It says nothing about a rule
/// file the gate could not read, so the escalation replaces the consent allow
/// with the safety ask; otherwise one earlier approval would silently cover a
/// file that may hold a deny rule.
#[test]
fn test_consent_cannot_cover() {
    let root = temp_root("consent");
    let consent = Arc::new(InMemoryConsentStore::non_expiring());
    consent.record("read", "");
    let gate = DefaultModeGate::new_without_builtins().with_consent(consent);
    let gate = gate.with_store(corrupt_store(&root));
    let decision = gate.decide(&req("read", false, true, false));
    let Decision::Ask(reason) = &decision else {
        panic!("a consent for the call must not cover the unread file: {decision:?}");
    };
    assert_eq!(reason.source, AskSource::SystemSafety);
    remove_dir_all(&root).ok();
}

/// The fenced-exec relaxation upgrades only the mode-default ask. The
/// escalation carries its own validator, so a fence that covers the call still
/// leaves the safety ask standing.
#[test]
fn test_fence_keeps_failure_ask() {
    struct StubFence;
    impl Containment for StubFence {
        fn coverage(&self) -> Coverage {
            Coverage::Fenced {
                writable_roots: vec![PathBuf::from("/ws")],
            }
        }
        fn would_block(&self, _effect: SideEffect) -> Option<String> {
            None
        }
    }

    let root = temp_root("fenced");
    let gate = DefaultModeGate::with_mode(PermissionMode::Auto)
        .with_containment(Arc::new(StubFence))
        .with_auto_allow_fenced_exec(true)
        .with_store(corrupt_store(&root));
    let decision = gate.decide(&req("bash", false, false, false));
    let Decision::Ask(reason) = &decision else {
        panic!("the fence may not relax the safety ask: {decision:?}");
    };
    assert_eq!(reason.source, AskSource::SystemSafety);
    remove_dir_all(&root).ok();
}
