use std::env;
use std::fs;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value;

use super::*;
use crate::decision::Outcome;
use crate::gate::{DefaultModeGate, ModeGate};
use crate::mode::ToolRequest;
use crate::rule::{Effect, Rule, RuleContent};
use crate::store::{RuleStore, Scope, StoreError};
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

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
    let loaded = store2.try_load().expect("load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].action, "bash");
    assert!(matches!(loaded[0].content, Some(RuleContent::Prefix(_))));
    assert_eq!(loaded[0].effect, Effect::Allow);
}

#[test]
fn test_store_union_across_scopes() {
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

    let loaded = store.try_load().expect("load");
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

    let loaded = store.try_load().expect("load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].action, "edit");
}

/// Clearing the rules keeps the directory authorizations. The two live in
/// one envelope, and a clear that rewrote the file from empty would drop the
/// sandbox grants the user made.
#[test]
fn test_clear_preserves_directories() {
    let dir = tempdir();
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let store = tmp_store(&dir, "clear-dirs");
    store
        .add(&Rule::new("bash", Effect::Allow).unwrap())
        .unwrap();
    store
        .add_directory(&workspace, Scope::Project)
        .expect("add_directory");

    store.clear().expect("clear");

    assert!(
        store.try_load().expect("load").is_empty(),
        "the rules are gone"
    );
    assert_eq!(
        store.try_load_directories().expect("directories"),
        vec![dunce::canonicalize(&workspace).unwrap_or(workspace)],
        "the directory grant survives the clear"
    );
}

/// A clear with no rules to drop is a no-op rather than a rewrite: the file
/// still carries the directory grants, and it is not touched at all.
#[test]
fn test_clear_skips_empty_rules() {
    let dir = tempdir();
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let store = tmp_store(&dir, "clear-empty");
    store
        .add_directory(&workspace, Scope::Project)
        .expect("add_directory");
    let path = dir.join("clear-empty").join("project.json");
    let before = fs::read(&path).expect("read the scope file");
    let modified = fs::metadata(&path)
        .expect("stat the scope file")
        .modified()
        .expect("a file modification time");

    store.clear().expect("clear");

    assert_eq!(
        fs::read(&path).expect("read the scope file"),
        before,
        "the file keeps its bytes"
    );
    assert_eq!(
        fs::metadata(&path)
            .expect("stat the scope file")
            .modified()
            .expect("a file modification time"),
        modified,
        "a clear with nothing to drop does not rewrite the file"
    );
    assert_eq!(
        store.try_load_directories().expect("directories"),
        vec![dunce::canonicalize(&workspace).unwrap_or(workspace)],
        "the directory grant survives the clear"
    );
}

#[test]
fn test_clear_wipes_write_scope() {
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
    let loaded = store.try_load().expect("load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].action, "read");
}

#[test]
fn test_gate_persists_allow_rule() {
    let dir = tempdir();
    let store = Arc::new(tmp_store(&dir, "e")) as Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store.clone());
    let rule =
        Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap();
    gate.add_rule(rule.clone()).expect("saved");

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
    let store = Arc::new(tmp_store(&dir, "f")) as Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store.clone());
    // A rule is an always-X directive, not a one-time verdict, so EVERY effect
    // (allow/deny/ask) persists to its chosen scope. The store holds the
    // deny rule; load() re-hydrates it on restart.
    gate.add_rule(Rule::new("bash", Effect::Deny).unwrap())
        .expect("saved");
    assert_eq!(store.try_load().expect("load").len(), 1);
}

#[test]
fn test_remove_rule_deletes_store() {
    // remove_rule must delete from the persistence layer at the rule's own
    // scope (symmetric with add), so a deleted rule does NOT re-hydrate from
    // the store on a fresh gate. Regression for the asymmetry where remove
    // hit write_scope (project) while a user-scope rule survived.
    let dir = tempdir();
    let store = Arc::new(tmp_store(&dir, "g")) as Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store.clone());
    // A rule persisted to USER scope (not the store's project write_scope).
    gate.add_rule(
        Rule::new("bash", Effect::Allow)
            .unwrap()
            .with_scope(Scope::User),
    )
    .expect("saved");
    assert_eq!(
        store.try_load().expect("load").len(),
        1,
        "added user-scope rule persists"
    );
    assert!(gate.remove_rule(0), "remove by index succeeds");
    assert!(
        store.try_load().expect("load").is_empty(),
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
    let dir = tempdir();
    let store = Arc::new(tmp_store(&dir, "h")) as Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store.clone());
    gate.add_rule(
        Rule::with_content("bash", RuleContent::Prefix("npm".into()), Effect::Allow).unwrap(),
    )
    .expect("saved");
    gate.add_rule(
        Rule::with_content("bash", RuleContent::Prefix("git".into()), Effect::Allow).unwrap(),
    )
    .expect("saved");
    assert_eq!(
        store.try_load().expect("load").len(),
        2,
        "both same-action rules persist"
    );
    // Remove index 0 (the npm rule). The git rule must survive.
    assert!(gate.remove_rule(0));
    let loaded = store.try_load().expect("load");
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
impl RuleStore for FailingStore {
    fn try_load(&self) -> Result<Vec<Rule>, StoreError> {
        Ok(Vec::new())
    }
    fn try_load_directories(&self) -> Result<Vec<PathBuf>, StoreError> {
        Ok(Vec::new())
    }
    fn try_load_read_directories(&self) -> Result<Vec<PathBuf>, StoreError> {
        Ok(Vec::new())
    }
    fn add(&self, _rule: &Rule) -> Result<(), StoreError> {
        Ok(())
    }
    fn remove(&self, _rule: &Rule) -> Result<(), StoreError> {
        Err(StoreError::Io)
    }
    fn clear(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

#[test]
fn test_remove_rule_persist_failure() {
    let store = Arc::new(FailingStore) as Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store);
    gate.add_rule(Rule::new("bash", Effect::Allow).unwrap())
        .expect("saved");
    assert_eq!(gate.rules().len(), 1);
    // remove_rule returns true (in-memory deletion succeeded) even though the
    // persistence write failed; the eprintln surfaces the disk divergence.
    assert!(gate.remove_rule(0));
    assert!(
        gate.rules().is_empty(),
        "in-memory rule still gone despite disk failure"
    );
}

/// A store whose every rule and directory write fails, so a durable grant
/// cannot land while the in-memory one can.
struct RefusingAddStore;
impl RuleStore for RefusingAddStore {
    fn try_load(&self) -> Result<Vec<Rule>, StoreError> {
        Ok(Vec::new())
    }
    fn try_load_directories(&self) -> Result<Vec<PathBuf>, StoreError> {
        Ok(Vec::new())
    }
    fn try_load_read_directories(&self) -> Result<Vec<PathBuf>, StoreError> {
        Ok(Vec::new())
    }
    fn add(&self, _rule: &Rule) -> Result<(), StoreError> {
        Err(StoreError::Io)
    }
    fn remove(&self, _rule: &Rule) -> Result<(), StoreError> {
        Ok(())
    }
    fn clear(&self) -> Result<(), StoreError> {
        Ok(())
    }
    fn add_directory(&self, _dir: &Path, _scope: Scope) -> Result<(), StoreError> {
        Err(StoreError::Io)
    }
    fn add_read_directory(&self, _dir: &Path, _scope: Scope) -> Result<(), StoreError> {
        Err(StoreError::Io)
    }
    fn remove_directory(&self, _dir: &Path, _scope: Scope) -> Result<(), StoreError> {
        Err(StoreError::Io)
    }
}

#[test]
fn test_save_failure_no_rule() {
    let store = Arc::new(RefusingAddStore) as Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store);
    let error = gate
        .add_rule(Rule::new("bash", Effect::Allow).unwrap())
        .expect_err("a rule the store refused must not report success");
    assert!(matches!(error, StoreError::Io));
    assert!(
        gate.rules().is_empty(),
        "a rule the store refused must not enter the live set either"
    );
}

/// A directory grant the store refuses reaches the caller as an error, while
/// a gate with no store accepts it: without a store the grant only has to
/// hold for this session, so there is nothing that could fail.
#[test]
fn test_directory_save_failure_reported() {
    let dir = tempdir();
    let target = dir.join("target");
    fs::create_dir_all(&target).expect("mkdir target");

    let session_gate = DefaultModeGate::new_without_builtins();
    assert!(
        session_gate.add_directory(&target, Scope::Local).is_ok(),
        "no store means nothing to save, so the add succeeds"
    );
    assert!(
        session_gate
            .add_read_directory(&target, Scope::Local)
            .is_ok(),
        "the read-only add behaves the same way"
    );
    assert!(
        session_gate.remove_directory(&target).is_ok(),
        "dropping a grant from a storeless gate is a no-op"
    );

    let store = Arc::new(RefusingAddStore) as Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store);
    assert!(
        matches!(
            gate.add_directory(&target, Scope::Local),
            Err(StoreError::Io)
        ),
        "the refused write is reported, not swallowed"
    );
    assert!(
        matches!(
            gate.add_read_directory(&target, Scope::Local),
            Err(StoreError::Io)
        ),
        "the refused read-only write is reported too"
    );
    assert!(
        matches!(gate.remove_directory(&target), Err(StoreError::Io)),
        "a removal the store cannot persist is reported"
    );
}

#[test]
fn test_load_stamps_scope_file() {
    // A legacy rule written to user.json before the scope field existed (no
    // scope in the JSON) deserializes to the serde default (Project). load()
    // must stamp the scope from the FILE it was read from, so remove_rule
    // finds it in user.json (not project) + deletes it — otherwise the rule
    // is silent-unremovable (no match in project → no-op Ok → re-hydrate).
    let dir = tempdir();
    let file_store = tmp_store(&dir, "i");
    // Write a bare legacy rule (no scope field) straight to user.json.
    let user_path = file_store.path_for(Scope::User).to_path_buf();
    fs::create_dir_all(user_path.parent().unwrap()).ok();
    fs::write(&user_path, r#"[{"action":"bash","effect":"Allow"}]"#).unwrap();
    let store = Arc::new(file_store) as Arc<dyn RuleStore>;
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
        store.try_load().expect("load").is_empty(),
        "legacy user-file rule must be removable, not silent-unremovable"
    );
}

#[test]
fn test_gate_without_store_unchanged() {
    // A gate with no store attached behaves exactly as before: rules live
    // only in memory and a fresh gate starts empty.
    let gate = DefaultModeGate::new_without_builtins();
    gate.add_rule(Rule::new("bash", Effect::Allow).unwrap())
        .expect("saved");
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
    let dir = tempdir();
    let store = Arc::new(tmp_store(&dir, "dirp")) as Arc<dyn RuleStore>;
    let target = env::temp_dir().join(format!("houyi-dir-target-{}", process::id()));
    fs::create_dir_all(&target).expect("mkdir target");
    store
        .add_directory(&target, Scope::Project)
        .expect("add_directory");
    // A fresh store pointing at the same files hydrates the directory.
    let store2 = Arc::new(tmp_store(&dir, "dirp")) as Arc<dyn RuleStore>;
    let dirs = store2.try_load_directories().expect("directories");
    let canonical = dunce::canonicalize(&target).expect("canonicalize target");
    assert!(
        dirs.iter().any(|d| d == &canonical),
        "directory must hydrate from disk into a fresh store: {dirs:?}"
    );
    // remove_directory drops it (symmetric with add) so a removed entry does
    // not re-hydrate on a fresh store.
    store2
        .remove_directory(&target, Scope::Project)
        .expect("remove");
    let store3 = Arc::new(tmp_store(&dir, "dirp")) as Arc<dyn RuleStore>;
    assert!(
        store3
            .try_load_directories()
            .expect("directories")
            .is_empty(),
        "removed directory must not re-hydrate"
    );
    fs::remove_dir_all(&target).ok();
}

/// A directory added through the gate is written to the store, and one
/// removed through the gate is dropped from it, so the pane's list and the
/// grants a restart hydrates stay one list.
#[test]
fn test_gate_persists_directory() {
    let dir = tempdir();
    let store = Arc::new(tmp_store(&dir, "gate-dir")) as Arc<dyn RuleStore>;
    let gate = DefaultModeGate::new_without_builtins().with_store(store);
    let target = dir.join("granted");
    fs::create_dir_all(&target).expect("mkdir target");
    let canonical = dunce::canonicalize(&target).expect("canonicalize target");

    gate.add_directory(&target, Scope::Local).expect("saved");
    let fresh = tmp_store(&dir, "gate-dir");
    assert!(
        fresh
            .try_load_directories()
            .expect("directories")
            .contains(&canonical),
        "a directory added through the gate must hydrate on the next start"
    );

    // The pane names whatever the user typed, which may not be the canonical
    // spelling, so the removal resolves the path the same way the add did.
    let spelled = format!("{}{}", target.display(), MAIN_SEPARATOR);
    gate.remove_directory(Path::new(&spelled)).expect("saved");
    let after = tmp_store(&dir, "gate-dir");
    assert!(
        after
            .try_load_directories()
            .expect("directories")
            .is_empty(),
        "a directory removed through the gate must not hydrate on the next start"
    );
}

/// A grant typed with a leading tilde is stored under the directory it names,
/// so the removal the pane sends afterward, which carries the expanded path,
/// still matches the record.
#[test]
fn test_directory_expands_typed_tilde() {
    let Some(home) = home_directory() else {
        return;
    };
    let dir = tempdir();
    let store = tmp_store(&dir, "tilde-dir");
    let leaf = format!("houyi-tilde-{}", process::id());
    let typed = PathBuf::from("~").join(&leaf);
    let expanded = home.join(&leaf);

    store.add_directory(&typed, Scope::Local).expect("add");
    let stored = tmp_store(&dir, "tilde-dir")
        .try_load_directories()
        .expect("directories");
    assert!(
        stored.iter().all(|path| !path.starts_with("~")),
        "a typed tilde must be stored expanded: {stored:?}"
    );

    store
        .remove_directory(&expanded, Scope::Local)
        .expect("remove");
    assert!(
        tmp_store(&dir, "tilde-dir")
            .try_load_directories()
            .expect("directories")
            .is_empty(),
        "the expanded spelling must remove the grant the tilde added"
    );
}

/// A directory an older writer persisted in the Windows verbatim form
/// hydrates back in the plain drive-letter form, so removal matches it and a
/// fence comparison holds. The verbatim prefix exists only on Windows, so
/// that is where the discrimination lives.
#[cfg(windows)]
#[test]
fn test_verbatim_dir_simplifies() {
    let dir = tempdir();
    let root = dir.join("legacy");
    fs::create_dir_all(&root).expect("legacy root");
    fs::write(
        root.join("project.json"),
        r#"{"version":2,"rules":[],"directories":["\\\\?\\C:\\houyi-legacy-dir"],"read_directories":[]}"#,
    )
    .expect("write legacy envelope");
    let store = tmp_store(&dir, "legacy");
    let dirs = store.try_load_directories().expect("directories");
    assert_eq!(
        dirs,
        vec![PathBuf::from("C:\\houyi-legacy-dir")],
        "verbatim entry simplifies on load: {dirs:?}"
    );
}

#[test]
fn test_read_directory_round_trip() {
    let dir = tempdir();
    let target = env::temp_dir().join(format!("houyi-read-dir-{}", process::id()));
    fs::create_dir_all(&target).unwrap();
    let store = Arc::new(tmp_store(&dir, "read-dir")) as Arc<dyn RuleStore>;
    store.add_read_directory(&target, Scope::Local).unwrap();
    let reloaded = Arc::new(tmp_store(&dir, "read-dir")) as Arc<dyn RuleStore>;
    assert_eq!(
        reloaded
            .try_load_read_directories()
            .expect("directories")
            .len(),
        1
    );
    assert!(
        reloaded
            .try_load_directories()
            .expect("directories")
            .is_empty()
    );
    reloaded.add_directory(&target, Scope::Local).unwrap();
    assert!(
        reloaded
            .try_load_read_directories()
            .expect("directories")
            .is_empty()
    );
    assert_eq!(
        reloaded.try_load_directories().expect("directories").len(),
        1
    );
    reloaded.remove_directory(&target, Scope::Local).unwrap();
    assert!(
        reloaded
            .try_load_directories()
            .expect("directories")
            .is_empty()
    );
    fs::remove_dir_all(target).ok();
}

/// Adding the same directory twice is idempotent (one entry, not two), and
/// removing a directory that was never added is a no-op (no panic, no write).
/// Covers the duplicate-skip and no-match early-return branches in
/// add_directory / remove_directory.
#[test]
fn test_add_idempotent_remove_absent() {
    let dir = tempdir();
    let store = Arc::new(tmp_store(&dir, "diridem")) as Arc<dyn RuleStore>;
    let target = env::temp_dir().join(format!("houyi-dir-idem-{}", process::id()));
    fs::create_dir_all(&target).expect("mkdir");
    store.add_directory(&target, Scope::Project).unwrap();
    store.add_directory(&target, Scope::Project).unwrap();
    assert_eq!(
        store.try_load_directories().expect("directories").len(),
        1,
        "duplicate add is idempotent"
    );
    // Removing a directory that was never added is a no-op.
    let absent = env::temp_dir().join(format!("houyi-dir-absent-{}", process::id()));
    store.remove_directory(&absent, Scope::Project).unwrap();
    assert_eq!(
        store.try_load_directories().expect("directories").len(),
        1,
        "remove-absent is a no-op"
    );
    fs::remove_dir_all(&target).ok();
}

/// A store that persists no directories answers both reads with an empty list
/// rather than an error, and the write methods a non-file store leaves alone
/// stay no-ops. Declaring nothing is a valid answer; failing to decode a file
/// is the only reason a read errors.
#[test]
fn test_empty_grants_skip_writes() {
    let store = Arc::new(FailingStore) as Arc<dyn RuleStore>;
    assert!(
        store
            .try_load_directories()
            .expect("no write grants")
            .is_empty()
    );
    assert!(
        store
            .try_load_read_directories()
            .expect("no read grants")
            .is_empty()
    );
    assert!(store.add_directory(Path::new("/x"), Scope::Project).is_ok());
    assert!(
        store
            .remove_directory(Path::new("/x"), Scope::Project)
            .is_ok()
    );
}

/// A per-call counter, so two tests that ask for a root in the same
/// microsecond cannot land in one directory and write over each other.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn tempdir() -> PathBuf {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!(
        "houyicoder-permission-{}-{seq}-{}",
        process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("tempdir");
    dir
}

/// Write the project scope file with the given bytes before the store opens,
/// returning the store and the path a caller can compare against.
fn seeded_store(dir: &Path, name: &str, bytes: &[u8]) -> (FileRuleStore, PathBuf) {
    let store = tmp_store(dir, name);
    let path = store.path_for(Scope::Project).to_path_buf();
    fs::create_dir_all(path.parent().expect("the scope directory")).expect("mkdir scope");
    fs::write(&path, bytes).expect("seed the scope file");
    (store, path)
}

/// Every write on a scope file this build cannot decode fails and leaves the
/// bytes alone. Initializing the file as empty would replace a deny rule the
/// build merely failed to parse with a set holding nothing.
#[test]
fn test_corrupt_store_blocks_writes() {
    let dir = tempdir();
    let (store, path) = seeded_store(&dir, "corrupt", b"{ this is not a permissions file");
    let before = fs::read(&path).expect("read the seeded bytes");
    let grant = dir.join("grant");
    fs::create_dir_all(&grant).expect("mkdir grant");
    let allow = Rule::new("bash", Effect::Allow).unwrap();
    let attempts = [
        ("add", store.add(&allow)),
        ("remove", store.remove(&allow)),
        ("clear", store.clear()),
        ("add directory", store.add_directory(&grant, Scope::Project)),
        (
            "add read directory",
            store.add_read_directory(&grant, Scope::Project),
        ),
        (
            "remove directory",
            store.remove_directory(&grant, Scope::Project),
        ),
    ];
    for (operation, outcome) in attempts {
        let error = outcome.expect_err("a write on an undecodable file must fail");
        assert!(
            matches!(error, StoreError::Unreadable { .. }),
            "{operation}: the failure names the file: {error}"
        );
        assert_eq!(
            fs::read(&path).expect("read the bytes after the refusal"),
            before,
            "{operation}: the file keeps its bytes"
        );
    }
    assert!(store.try_load().is_err(), "the read reports the same file");
    fs::remove_dir_all(&dir).ok();
}

/// A zero-byte file is not an empty rule set. It is what a crashed or
/// disk-full writer left behind, and the rules it was meant to hold are
/// unknown, so the store reports it until the user deletes it.
#[test]
fn test_empty_store_blocks_writes() {
    let dir = tempdir();
    let (store, path) = seeded_store(&dir, "zero", b"");
    let error = store
        .add(&Rule::new("bash", Effect::Allow).unwrap())
        .expect_err("a zero-byte file is not an empty rule set");
    let message = error.to_string();
    assert!(
        message.contains(&path.display().to_string()),
        "the failure names the file: {message}"
    );
    assert!(
        message.contains("no bytes"),
        "the failure tells the user what happened to the file: {message}"
    );
    assert!(
        fs::read(&path)
            .expect("read the bytes after the refusal")
            .is_empty(),
        "the file keeps its zero bytes"
    );
    assert!(store.try_load().is_err(), "a load of it fails too");
    fs::remove_dir_all(&dir).ok();
}

/// A path that exists but cannot be opened, here a directory where the scope
/// file belongs, is reported rather than treated as an absent file.
#[test]
fn test_directory_path_blocks_writes() {
    let dir = tempdir();
    let store = tmp_store(&dir, "dirpath");
    let path = store.path_for(Scope::Project).to_path_buf();
    fs::create_dir_all(&path).expect("a directory in the file's place");
    let error = store
        .add(&Rule::new("bash", Effect::Allow).unwrap())
        .expect_err("a directory cannot be read as a permissions file");
    assert!(
        matches!(error, StoreError::Unreadable { .. }),
        "the failure names the path: {error}"
    );
    assert!(path.is_dir(), "the path is left as it was");
    fs::remove_dir_all(&dir).ok();
}

/// A version newer than this build reads may carry rules this build cannot
/// represent. Rewriting it would drop them, so reads and writes both refuse
/// and the file keeps every member it had.
#[test]
fn test_unsupported_version_blocks_writes() {
    for version in [0u32, 99u32] {
        let dir = tempdir();
        let body = format!(
            r#"{{"version":{version},"rules":[{{"action":"bash","effect":"Deny"}}],"directories":[]}}"#
        );
        let (store, path) = seeded_store(&dir, "version", body.as_bytes());
        let error = store
            .add(&Rule::new("bash", Effect::Allow).unwrap())
            .expect_err("an envelope outside the readable range cannot be rewritten");
        assert!(
            error.to_string().contains(&version.to_string()),
            "the failure names the version it read: {error}"
        );
        assert_eq!(
            fs::read(&path).expect("read the bytes after the refusal"),
            body.as_bytes(),
            "a file this build cannot read keeps its bytes"
        );
        assert!(store.try_load().is_err(), "the read refuses it too");
        fs::remove_dir_all(&dir).ok();
    }
}

/// The envelope-less shape older builds wrote is still a rule set: a bare
/// array decodes, with each rule taking the scope of the file it came from.
#[test]
fn test_legacy_rules_still_load() {
    let dir = tempdir();
    let (store, _path) = seeded_store(&dir, "legacy", br#"[{"action":"bash","effect":"Deny"}]"#);
    let loaded = store.try_load().expect("a bare array is readable");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].effect, Effect::Deny);
    assert_eq!(loaded[0].scope, Scope::Project);
    fs::remove_dir_all(&dir).ok();
}

/// A version 1 envelope lacks the read-directory list and is still accepted;
/// the next write stamps the current version while keeping both rules.
#[test]
fn test_old_envelope_loads() {
    let dir = tempdir();
    let (store, path) = seeded_store(
        &dir,
        "old",
        br#"{"version":1,"rules":[{"action":"bash","effect":"Deny"}],"directories":[]}"#,
    );
    let loaded = store.try_load().expect("a version 1 envelope is readable");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].effect, Effect::Deny);
    store
        .add(&Rule::new("read", Effect::Allow).unwrap())
        .expect("a write on a readable old file works");
    let after = store.try_load().expect("load the rewritten file");
    assert_eq!(after.len(), 2, "the old rule and the new one both survive");
    let text = fs::read_to_string(&path).expect("read the scope file");
    assert!(
        text.contains("\"version\":2"),
        "the rewrite stamps the current version: {text}"
    );
    fs::remove_dir_all(&dir).ok();
}

/// A member this build does not model belongs to another build's file. A
/// rewrite for a rule add carries it back unchanged.
#[test]
fn test_unknown_member_survives_add() {
    let dir = tempdir();
    let (store, path) = seeded_store(
        &dir,
        "future-member",
        br#"{"version":2,"rules":[{"action":"bash","effect":"Deny","from_another_build":{"keep":true}}],"directories":[],"from_another_build":{"keep":true}}"#,
    );
    store
        .add(&Rule::new("read", Effect::Allow).unwrap())
        .expect("add a rule");
    let text = fs::read_to_string(&path).expect("read the scope file");
    let value: Value = serde_json::from_str(&text).expect("the file is JSON");
    assert_eq!(
        value["from_another_build"]["keep"], true,
        "the envelope member this build does not model is written back: {text}"
    );
    assert_eq!(
        value["rules"][0]["from_another_build"]["keep"], true,
        "the rule member this build does not model is written back: {text}"
    );
    assert_eq!(
        store.try_load().expect("load").len(),
        2,
        "both rules are there as well"
    );
    fs::remove_dir_all(&dir).ok();
}

/// A store whose files are all absent is the first run: it reads as no rules
/// and the first write creates the file.
#[test]
fn test_absent_store_loads_empty() {
    let dir = tempdir();
    let store = tmp_store(&dir, "absent");
    assert!(
        store
            .try_load()
            .expect("no files means no rules")
            .is_empty(),
        "an absent store holds nothing"
    );
    store
        .add(&Rule::new("bash", Effect::Allow).unwrap())
        .expect("the first write creates the file");
    assert_eq!(store.try_load().expect("load").len(), 1);
    fs::remove_dir_all(&dir).ok();
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
    let loaded = store2.try_load().expect("load");
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
    let loaded = store2.try_load().expect("load");
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
    let loaded = tmp_store(&dir, "rmcase").try_load().expect("load");
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
    let loaded = tmp_store(&dir, "rmabsent").try_load().expect("load");
    assert_eq!(
        loaded.len(),
        1,
        "absent remove must leave the present rule intact"
    );
}
