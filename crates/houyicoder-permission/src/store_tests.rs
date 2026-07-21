use super::*;
use crate::decision::Outcome;
use crate::gate::{DefaultModeGate, ModeGate};
use crate::mode::ToolRequest;
use crate::rule::{Effect, Rule, RuleContent};
use serde_json::Value;

fn tmp_store(dir: &Path, name: &str) -> FileRuleStore {
    let root = dir.join(name);
    FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    )
}

#[test]
fn test_file_store_round_trip() {
    let dir = tempdir();
    let store = tmp_store(&dir, "a");
    let rule =
        Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
    store.add(&rule).expect("add");

    // A fresh store pointing at the same paths sees the rule.
    let store2 = tmp_store(&dir, "a");
    let loaded = store2.load();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].action, "bash");
    assert!(matches!(loaded[0].content, Some(RuleContent::Prefix(_))));
    assert_eq!(loaded[0].effect, Effect::Allow);
}

#[test]
fn test_store_union_across_scopes() {
    use crate::store::Scope;
    let dir = tempdir();
    let store = tmp_store(&dir, "b");
    // One allow rule in each scope (scope rides on the rule now, not the
    // store's write_scope); load returns all three.
    let user_rule = Rule::new("read", Effect::Allow)
        .unwrap()
        .with_scope(Scope::User);
    let project_rule = Rule::new("edit", Effect::Allow).unwrap();
    let local_rule = Rule::new("fetch", Effect::Allow)
        .unwrap()
        .with_scope(Scope::Local);

    store.add(&user_rule).unwrap();
    store.add(&project_rule).unwrap();
    store.add(&local_rule).unwrap();

    let loaded = store.load();
    assert_eq!(loaded.len(), 3);
    let actions: Vec<&str> = loaded.iter().map(|r| r.action.as_str()).collect();
    assert!(actions.contains(&"read"));
    assert!(actions.contains(&"edit"));
    assert!(actions.contains(&"fetch"));
}

#[test]
fn test_remove_drops_action() {
    let dir = tempdir();
    let store = tmp_store(&dir, "c");
    store
        .add(&Rule::new("bash", Effect::Allow).unwrap())
        .unwrap();
    store
        .add(&Rule::new("edit", Effect::Allow).unwrap())
        .unwrap();
    store
        .remove(&Rule::new("bash", Effect::Allow).unwrap())
        .expect("remove");

    let loaded = store.load();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].action, "edit");
}

#[test]
fn test_clear_wipes_write_scope() {
    use crate::store::Scope;
    let dir = tempdir();
    let store = tmp_store(&dir, "d");
    store
        .add(&Rule::new("bash", Effect::Allow).unwrap())
        .unwrap();
    store
        .add(
            &Rule::new("read", Effect::Allow)
                .unwrap()
                .with_scope(Scope::User),
        )
        .unwrap();

    // Clear only the project scope; the user scope keeps its rule.
    store.clear().unwrap();
    let loaded = store.load();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].action, "read");
}

#[test]
fn test_gate_persists_allow_rule() {
    let dir = tempdir();
    let store = std::sync::Arc::new(tmp_store(&dir, "e")) as std::sync::Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store.clone());
    let rule =
        Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
    gate.add_rule(rule.clone());

    // A fresh gate pointing at the same files hydrates the persisted rule
    // and grants the matching call without re-asking.
    let gate2 = DefaultModeGate::new_without_builtins().with_store(store);
    assert_eq!(gate2.rules().len(), 1);

    let v: &'static Value = Box::leak(serde_json::json!({"command": "npm install"}).into());
    let request = ToolRequest {
        tool_name: "bash",
        input: Some(v),
        is_destructive: false,
        is_read_only: false,
        native_requires_approval: false,
    };
    // The gate is in Default mode; the matched allow rule overrides the
    // default Ask for exec. The destructive gate does not fire because
    // the command is a plain npm install (no rm, no redirect).
    let decision = gate2.decide(&request);
    assert!(
        decision.outcome() == Outcome::Allow,
        "expected Allow, got {decision:?}"
    );
}

#[test]
fn test_gate_persists_deny() {
    let dir = tempdir();
    let store = std::sync::Arc::new(tmp_store(&dir, "f")) as std::sync::Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store.clone());
    // A rule is an always-X directive, not a one-time verdict, so EVERY effect
    // (allow/deny/ask) persists to its chosen scope. The store holds the
    // deny rule; load() re-hydrates it on restart.
    gate.add_rule(Rule::new("bash", Effect::Deny).unwrap());
    assert_eq!(store.load().len(), 1);
}

#[test]
fn test_remove_rule_deletes_store() {
    // remove_rule must delete from the persistence layer at the rule's own
    // scope (symmetric with add), so a deleted rule does NOT re-hydrate from
    // the store on a fresh gate. Regression for the asymmetry where remove
    // hit write_scope (project) while a user-scope rule survived.
    use crate::store::Scope;
    let dir = tempdir();
    let store = std::sync::Arc::new(tmp_store(&dir, "g")) as std::sync::Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store.clone());
    // A rule persisted to USER scope (not the store's project write_scope).
    gate.add_rule(
        Rule::new("bash", Effect::Allow)
            .unwrap()
            .with_scope(Scope::User),
    );
    assert_eq!(store.load().len(), 1, "added user-scope rule persists");
    assert!(gate.remove_rule(0), "remove by index succeeds");
    assert!(
        store.load().is_empty(),
        "remove must delete the user-scope rule from the store, not just memory"
    );
    // A fresh gate does not re-hydrate the deleted rule.
    let gate2 = DefaultModeGate::new_without_builtins().with_store(store);
    assert!(
        gate2.rules().is_empty(),
        "deleted rule must not come back on restart"
    );
}

#[test]
fn test_remove_keeps_sibling() {
    // A rule set has many same-action rules (bash npm:* + bash git:* both have
    // action="bash"). remove_rule must delete ONLY the one at the index — by
    // full identity (action + content + effect + scope) — not every same-
    // action sibling (the old remove-by-action wiped the whole family +
    // left memory/disk divergent). Two same-action rules: remove one, the
    // other survives in memory AND on disk.
    use crate::rule::RuleContent;
    let dir = tempdir();
    let store = std::sync::Arc::new(tmp_store(&dir, "h")) as std::sync::Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store.clone());
    gate.add_rule(
        Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap(),
    );
    gate.add_rule(
        Rule::with_content("bash", RuleContent::Prefix("git".into()), Effect::Allow).unwrap(),
    );
    assert_eq!(store.load().len(), 2, "both same-action rules persist");
    // Remove index 0 (the npm rule). The git rule must survive.
    assert!(gate.remove_rule(0));
    let loaded = store.load();
    assert_eq!(
        loaded.len(),
        1,
        "only the targeted rule is deleted, not its sibling"
    );
    assert_eq!(loaded[0].action, "bash");
    assert!(
        matches!(loaded[0].content, Some(RuleContent::Prefix(ref s)) if s == "git"),
        "the surviving rule is the git one, not the deleted npm one"
    );
    // Memory matches disk: one rule left, the right one.
    assert_eq!(gate.rules().len(), 1);
}

/// A store whose remove always fails, so remove_rule hits the error-log
/// branch. The in-memory rule is still gone (remove_rule returns true + the
/// memory Vec no longer holds it); the disk write failed so it would
/// re-hydrate on restart — the eprintln surfaces that signal.
struct FailingStore;
impl crate::store::RuleStore for FailingStore {
    fn load(&self) -> Vec<Rule> {
        Vec::new()
    }
    fn add(&self, _rule: &Rule) -> Result<(), crate::store::StoreError> {
        Ok(())
    }
    fn remove(&self, _rule: &Rule) -> Result<(), crate::store::StoreError> {
        Err(crate::store::StoreError::Io)
    }
    fn clear(&self) -> Result<(), crate::store::StoreError> {
        Ok(())
    }
}

#[test]
fn test_remove_rule_persist_failure() {
    let store = std::sync::Arc::new(FailingStore) as std::sync::Arc<dyn crate::store::RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store);
    gate.add_rule(Rule::new("bash", Effect::Allow).unwrap());
    assert_eq!(gate.rules().len(), 1);
    // remove_rule returns true (in-memory deletion succeeded) even though the
    // persistence write failed; the eprintln surfaces the disk divergence.
    assert!(gate.remove_rule(0));
    assert!(
        gate.rules().is_empty(),
        "in-memory rule still gone despite disk failure"
    );
}

#[test]
fn test_load_stamps_scope_file() {
    // A legacy rule written to user.json before the scope field existed (no
    // scope in the JSON) deserializes to the serde default (Project). load()
    // must stamp the scope from the FILE it was read from, so remove_rule
    // finds it in user.json (not project) + deletes it — otherwise the rule
    // is silent-unremovable (no match in project → no-op Ok → re-hydrate).
    use crate::store::Scope;
    let dir = tempdir();
    let file_store = tmp_store(&dir, "i");
    // Write a bare legacy rule (no scope field) straight to user.json.
    let user_path = file_store.path_for(Scope::User).to_path_buf();
    std::fs::create_dir_all(user_path.parent().unwrap()).ok();
    std::fs::write(&user_path, r#"[{"action":"bash","effect":"Allow"}]"#).unwrap();
    let store = std::sync::Arc::new(file_store) as std::sync::Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store.clone());
    let loaded = gate.rules();
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        loaded[0].scope,
        Scope::User,
        "load stamps scope from the file location, not the missing serialized field"
    );
    // remove_rule finds it in user.json (not project) + deletes it.
    assert!(gate.remove_rule(0));
    assert!(
        store.load().is_empty(),
        "legacy user-file rule must be removable, not silent-unremovable"
    );
}

#[test]
fn test_gate_without_store_unchanged() {
    // A gate with no store attached behaves exactly as before: rules live
    // only in memory and a fresh gate starts empty.
    let gate = DefaultModeGate::new_without_builtins();
    gate.add_rule(Rule::new("bash", Effect::Allow).unwrap());
    assert_eq!(gate.rules().len(), 1);

    let gate2 = DefaultModeGate::new_without_builtins();
    assert!(gate2.rules().is_empty());
}

/// Effect-level: a directory authorization persists to disk and hydrates
/// back from a fresh store. This is the persistence round-trip for the
/// directory list (1:1 with the sandbox fence's additional_dirs), not an
/// intent assertion. remove_directory must drop it so a deleted entry does
/// not re-hydrate.
#[test]
fn test_directory_persists_and_hydrates() {
    use crate::store::Scope;
    let dir = tempdir();
    let store = std::sync::Arc::new(tmp_store(&dir, "dirp")) as std::sync::Arc<dyn RuleStore>;
    let target = std::env::temp_dir().join(format!("houyi-dir-target-{}", std::process::id()));
    std::fs::create_dir_all(&target).expect("mkdir target");
    store
        .add_directory(&target, Scope::Project)
        .expect("add_directory");
    // A fresh store pointing at the same files hydrates the directory.
    let store2 = std::sync::Arc::new(tmp_store(&dir, "dirp")) as std::sync::Arc<dyn RuleStore>;
    let dirs = store2.load_directories();
    let canonical = std::fs::canonicalize(&target).expect("canonicalize target");
    assert!(
        dirs.iter().any(|d| d == &canonical),
        "directory must hydrate from disk into a fresh store: {dirs:?}"
    );
    // remove_directory drops it (symmetric with add) so a removed entry does
    // not re-hydrate on a fresh store.
    store2
        .remove_directory(&target, Scope::Project)
        .expect("remove");
    let store3 = std::sync::Arc::new(tmp_store(&dir, "dirp")) as std::sync::Arc<dyn RuleStore>;
    assert!(
        store3.load_directories().is_empty(),
        "removed directory must not re-hydrate"
    );
    std::fs::remove_dir_all(&target).ok();
}

/// Adding the same directory twice is idempotent (one entry, not two), and
/// removing a directory that was never added is a no-op (no panic, no write).
/// Covers the duplicate-skip and no-match early-return branches in
/// add_directory / remove_directory.
#[test]
fn test_add_idempotent_remove_absent() {
    use crate::store::Scope;
    let dir = tempdir();
    let store = std::sync::Arc::new(tmp_store(&dir, "diridem")) as std::sync::Arc<dyn RuleStore>;
    let target = std::env::temp_dir().join(format!("houyi-dir-idem-{}", std::process::id()));
    std::fs::create_dir_all(&target).expect("mkdir");
    store.add_directory(&target, Scope::Project).unwrap();
    store.add_directory(&target, Scope::Project).unwrap();
    assert_eq!(
        store.load_directories().len(),
        1,
        "duplicate add is idempotent"
    );
    // Removing a directory that was never added is a no-op.
    let absent = std::env::temp_dir().join(format!("houyi-dir-absent-{}", std::process::id()));
    store.remove_directory(&absent, Scope::Project).unwrap();
    assert_eq!(
        store.load_directories().len(),
        1,
        "remove-absent is a no-op"
    );
    std::fs::remove_dir_all(&target).ok();
}

/// The trait default directory methods (load_directories=empty,
/// add_directory/remove_directory=Ok) are no-ops on a non-file-backed store
/// (FailingStore). A store impl that does not override them must not break
/// the rehydrate path — the defaults must be inert.
#[test]
fn test_directory_default_methods_inert() {
    use crate::store::Scope;
    let store = std::sync::Arc::new(FailingStore) as std::sync::Arc<dyn RuleStore>;
    assert!(store.load_directories().is_empty());
    assert!(
        store
            .add_directory(std::path::Path::new("/x"), Scope::Project)
            .is_ok()
    );
    assert!(
        store
            .remove_directory(std::path::Path::new("/x"), Scope::Project)
            .is_ok()
    );
}

fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "houyicoder-permission-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("tempdir");
    dir
}

#[test]
fn test_add_same_rule_dedups() {
    let dir = tempdir();
    let store = tmp_store(&dir, "dedup");
    let rule =
        Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
    store.add(&rule).expect("add 1");
    store.add(&rule).expect("add 2 (should be no-op)");
    let store2 = tmp_store(&dir, "dedup");
    let loaded = store2.load();
    assert_eq!(loaded.len(), 1, "dedup: file should have 1 rule");
}

#[test]
fn test_add_case_dedups() {
    let dir = tempdir();
    let store = tmp_store(&dir, "case");
    let r1 = Rule::with_content("Bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
    let r2 = Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
    store.add(&r1).expect("add Bash");
    store
        .add(&r2)
        .expect("add bash (should dedup, case-insensitive)");
    let store2 = tmp_store(&dir, "case");
    let loaded = store2.load();
    assert_eq!(loaded.len(), 1, "Bash and bash are the same rule");
}

#[test]
fn test_remove_case_insensitive_matches() {
    let dir = tempdir();
    let store = tmp_store(&dir, "rmcase");
    let r1 = Rule::with_content("Bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
    store.add(&r1).expect("add Bash(npm:*)");
    let r2 = Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
    store
        .remove(&r2)
        .expect("remove bash(npm:*) should match Bash");
    let loaded = tmp_store(&dir, "rmcase").load();
    assert_eq!(
        loaded.len(),
        0,
        "case-insensitive remove must delete the rule"
    );
}

#[test]
fn test_remove_absent_is_noop() {
    // Removing a rule that is not in the file is a no-op: Ok + no write.
    // Covers the no-match early-return (the skip path that must not touch the
    // file, same reasoning as the add no-op skip — no needless rename churn
    // racing a concurrent reader).
    use crate::store::Scope;
    let dir = tempdir();
    let store = tmp_store(&dir, "rmabsent");
    // (a) Remove from a file that does not exist yet -> Ok, no file created.
    let absent =
        Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
    store
        .remove(&absent)
        .expect("remove from missing file is Ok");
    let project_path = store.path_for(Scope::Project).to_path_buf();
    assert!(
        !project_path.exists(),
        "no-op remove must not create the file"
    );
    // (b) Remove an absent rule from a file holding a different rule -> Ok,
    // the present rule survives (the no-op did not wipe the envelope).
    let other =
        Rule::with_content("bash", RuleContent::Prefix("git".into()), Effect::Allow).unwrap();
    store.add(&other).expect("add git rule");
    store.remove(&absent).expect("remove absent npm rule is Ok");
    let loaded = tmp_store(&dir, "rmabsent").load();
    assert_eq!(
        loaded.len(),
        1,
        "absent remove must leave the present rule intact"
    );
}
