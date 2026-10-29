//! Rule matching across a leading move into the session root.

use super::*;
use crate::decision::Outcome;
use crate::rule::{Effect, Rule, RuleContent};
use crate::store::Scope;
use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
use std::env::temp_dir;
use std::fs::{create_dir_all, remove_dir_all};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A fence narrowed to one root, which is what the rule spelling is compared
/// against.
struct RootFence(Arc<Path>);

impl Containment for RootFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![self.0.to_path_buf()],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn boundary_root(&self) -> Option<Arc<Path>> {
        Some(Arc::clone(&self.0))
    }
}

fn scratch_root(label: &str) -> PathBuf {
    let path = temp_dir().join(format!(
        "hy-rule-cd-{label}-{}-{}",
        process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    create_dir_all(&path).expect("mkdir scratch");
    path
}

fn manual_gate(root: &Path, rule: Rule) -> DefaultModeGate {
    let gate = DefaultModeGate::with_mode(PermissionMode::Manual)
        .with_containment(Arc::new(RootFence(Arc::from(root.to_path_buf()))));
    gate.add_rule(rule.with_scope(Scope::Session))
        .expect("saved");
    gate
}

#[test]
fn test_rule_allows_after_move() {
    let root = scratch_root("inside");
    let other = scratch_root("other");
    let gate = manual_gate(
        &root,
        Rule::with_content(
            "bash",
            RuleContent::Prefix("cargo build".into()),
            Effect::Allow,
        )
        .expect("rule"),
    );
    assert_eq!(
        gate.decide(&bash_req("cargo build")).outcome(),
        Outcome::Allow
    );
    let inside = format!("cd {} && cargo build", root.display());
    assert_eq!(
        gate.decide(&bash_req(&inside)).outcome(),
        Outcome::Allow,
        "{inside}"
    );
    let elsewhere = format!("cd {} && cargo build", other.display());
    assert_eq!(
        gate.decide(&bash_req(&elsewhere)).outcome(),
        Outcome::Ask,
        "{elsewhere}"
    );
    let semicolon = format!("cd {}; cargo build", root.display());
    assert_eq!(
        gate.decide(&bash_req(&semicolon)).outcome(),
        Outcome::Ask,
        "{semicolon}"
    );
    drop(remove_dir_all(&root));
    drop(remove_dir_all(&other));
}

#[test]
fn test_rule_denies_after_move() {
    let root = scratch_root("deny");
    let other = scratch_root("other");
    let gate = manual_gate(
        &root,
        Rule::with_content(
            "bash",
            RuleContent::Prefix("cargo build".into()),
            Effect::Deny,
        )
        .expect("rule"),
    );
    let inside = format!("cd {} && cargo build", root.display());
    assert_eq!(
        gate.decide(&bash_req(&inside)).outcome(),
        Outcome::Deny,
        "{inside}"
    );
    let elsewhere = format!("cd {} && cargo build", other.display());
    assert_eq!(
        gate.decide(&bash_req(&elsewhere)).outcome(),
        Outcome::Ask,
        "{elsewhere}"
    );
    drop(remove_dir_all(&root));
    drop(remove_dir_all(&other));
}
