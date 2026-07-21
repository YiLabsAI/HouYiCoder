//! Persistent rule storage. The in-memory gate is the single source of truth
//! for the live process; this module is the persistence seam that survives a
//! restart. Three scopes follow the layered settings hierarchy: user (home,
//! shared across projects), project (repo root, checked into the repo), and
//! local (a runtime temp path, ephemeral). A new process that never held the
//! rules in memory hydrates from these files on construction.
//!
//! Merge policy for v1 is union: rules from all three scopes combine into one
//! flat list, and the gate's existing last-match-wins + deny-wins evaluator
//! resolves order at decision time. Cross-scope precedence (a project rule
//! overriding a user rule for the same action) is intentionally NOT resolved
//! at this layer for v1 — union keeps the store a pure persistence seam and
//! avoids a second precedence model drifting from the evaluator. A later
//! revision can layer precedence on top of this contract without changing the
//! trait shape.
//!
//! Write scope: add, remove, and clear operate on ONE scope at a time (the
//! store's configured write scope, project by default). A write never edits
//! a higher-authority scope out from under the caller — a project context
//! never silently edits the user's home file. Load still returns the union,
//! so a rule persisted to the project scope is visible alongside user rules.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::rule::Rule;

/// The persistence scopes, ordered by ascending authority breadth. User is
/// the broadest (one home file shared across projects); Local is the
/// narrowest persisted scope (a machine-private, gitignored file under the
/// project dot directory — private to this machine, never checked in, but
/// survives a restart, the analog of a settings.local.json). Project is the
/// default write scope for an interactive always-allow: it is checked into
/// the repo and shared with collaborators. Builtin is not a persistence
/// destination: it marks rules that ship with the binary and are seeded into
/// the rule set at construction, never written to disk. Session is the
/// in-memory scope: a rule the user consented to for this process only
/// (cleared on restart). "don't ask again for this session" lives here,
/// not on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Scope {
    User,
    Project,
    Local,
    Session,
    Builtin,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
            Self::Session => "session",
            Self::Builtin => "builtin",
        }
    }

    /// Whether this scope is writable to disk. Session and Builtin are not:
    /// Session is in-memory (cleared on restart), Builtin is seeded at
    /// construction, so add_rule / persist must skip both.
    pub fn is_writable(self) -> bool {
        !matches!(self, Self::Session | Self::Builtin)
    }
}

/// The default write scope: project (repo-shared), so a rule constructed
/// without an explicit scope lands in the checked-in file.
impl Default for Scope {
    fn default() -> Self {
        Self::Project
    }
}

/// Failures a rule store can report. Io covers read, write, rename, and
/// directory creation failures; Decode and Encode cover serialization.
#[derive(Debug)]
pub enum StoreError {
    Io,
    Decode,
    Encode,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io => f.write_str("rule store I/O failure"),
            Self::Decode => f.write_str("rule store decode failure"),
            Self::Encode => f.write_str("rule store encode failure"),
        }
    }
}

impl std::error::Error for StoreError {}

/// The persistence seam for durable rules. Object-safe so the gate holds a
/// single shared dispatcher. The in-memory fast path skips this entirely; an
/// impl is attached when an always-allow verdict should survive a restart.
///
/// The contract is single-writer: the composition root serializes writes to
/// a given scope (one interactive verdict at a time owns the gate). A
/// multi-writer impl needs an external lock and is deferred.
///
/// load returns the union of all scopes; add, remove, and clear operate on
/// the store's configured write scope only.
pub trait RuleStore: Send + Sync {
    /// Read the union of rules across all configured scopes. Missing files
    /// contribute no rules; a corrupt file is skipped (best-effort) so a
    /// single bad scope cannot brick the gate on startup.
    fn load(&self) -> Vec<Rule>;

    /// Append a rule to the rule's own scope's file (the rule carries its
    /// destination, not the store's configured write_scope). The file is
    /// created when missing and rewritten atomically (tmp + rename) so a
    /// reader never sees a partial file. All effects (allow/deny/ask)
    /// persist — a rule is an always-X directive, not a one-time verdict.
    /// Idempotent: a rule whose identity already exists is a no-op (no
    /// write). Identity is case-insensitive on action and exact on content,
    /// effect, and scope — the same judgment remove uses (centralized in
    /// Rule::same_as, reachable via the pub Rule type); implementations
    /// MUST share it or add and remove drift apart.
    fn add(&self, rule: &Rule) -> Result<(), StoreError>;

    /// Remove every rule matching the given identity from the rule's own
    /// scope's file (the rule carries its destination, not the store's
    /// write_scope). Identity is case-insensitive on action and exact on
    /// content, effect, and scope — the same judgment add uses, so the pair
    /// stays symmetric. Pre-existing duplicates collapse to zero (dedup
    /// prevents new, not existing). Missing file or no match is a no-op.
    fn remove(&self, rule: &Rule) -> Result<(), StoreError>;

    /// Drop every rule from the write scope's file. Other scopes are
    /// untouched (a project clear never wipes the user's home file).
    fn clear(&self) -> Result<(), StoreError>;

    /// Read the union of directory authorizations across all configured
    /// scopes. Directories are 1:1 with the sandbox fence's additional_dirs:
    /// the startup path rehydrates them into the fence so a directory the user
    /// authorized survives restart. Default returns empty so a non-file impl
    /// (a test mock) is unchanged.
    fn load_directories(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Append a directory authorization to the given scope's envelope. Atomic
    /// (tmp + rename) like add. Default no-op so non-file impls stay unchanged.
    fn add_directory(&self, _dir: &Path, _scope: Scope) -> Result<(), StoreError> {
        Ok(())
    }

    /// Remove a directory authorization from the given scope. No-op when
    /// absent. Default no-op.
    fn remove_directory(&self, _dir: &Path, _scope: Scope) -> Result<(), StoreError> {
        Ok(())
    }
}

/// A file-backed RuleStore: one JSON file per scope, each holding a Vec of
/// rules. Atomic write via tmp + rename so a hydrating reader never sees a
/// half-written file. The write scope is configurable; the default is
/// Project so an interactive always-allow lands in the repo-shared file.
/// Callers share a single store via Arc<dyn RuleStore>, so this struct is
/// not Clone.
pub struct FileRuleStore {
    paths: HashMap<Scope, PathBuf>,
    write_scope: Scope,
    lock: Mutex<()>,
}

impl FileRuleStore {
    /// Open a store rooted at the three given paths. Each path is the
    /// concrete file path for its scope (not a directory); parent
    /// directories are created on first write. The default write scope is
    /// Project.
    pub fn new(user: PathBuf, project: PathBuf, local: PathBuf) -> Self {
        let mut paths = HashMap::new();
        paths.insert(Scope::User, user);
        paths.insert(Scope::Project, project);
        paths.insert(Scope::Local, local);
        Self {
            paths,
            write_scope: Scope::Project,
            lock: Mutex::new(()),
        }
    }

    /// Use conventional default paths for the three scopes. User points at
    /// a dot directory under the home dir; Project at a dot directory under
    /// the current dir; Local at a machine-private, gitignored file under
    /// the project dot directory (the analog of a settings.local.json —
    /// private to this machine, persisted across restart, never checked in).
    /// The home root is resolved from the environment at call time so a test
    /// can stub it.
    pub fn default_paths() -> Self {
        let user = home_dir().join(".houyicoder").join("permissions.json");
        // Resolve Project + Local to absolute paths so a worktree cwd change
        // (worktree enter/exit) does not silently redirect the store to a
        // different dot directory and lose authorizations.
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let project = cwd.join(".houyicoder").join("permissions.json");
        let local = cwd.join(".houyicoder").join("permissions.local.json");
        Self::new(user, project, local)
    }

    /// Change the scope that add, remove, and clear write to. Returns the
    /// store for chaining. A user-level always-allow (a rule that should
    /// apply across every project) is persisted by switching to User first.
    pub fn with_write_scope(mut self, scope: Scope) -> Self {
        self.write_scope = scope;
        self
    }

    /// The scope the store currently writes to.
    pub fn write_scope(&self) -> Scope {
        self.write_scope
    }

    fn path_for(&self, scope: Scope) -> &Path {
        self.paths
            .get(&scope)
            .expect("every scope has a path at construction")
    }

    fn read_scope(&self, scope: Scope) -> Vec<Rule> {
        self.read_envelope(scope).rules
    }

    /// Write the envelope without taking the lock. The caller MUST hold
    /// self.lock across the read-modify-write so two concurrent writers (a
    /// rule add from /permissions + a directory add from an approval) do not
    /// clobber each other's half of the shared file.
    fn write_envelope_unlocked(
        &self,
        scope: Scope,
        env: &PermissionsFile,
    ) -> Result<(), StoreError> {
        let path = self.path_for(scope);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| StoreError::Io)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(env).map_err(|_| StoreError::Encode)?;
        std::fs::write(&tmp, bytes).map_err(|_| StoreError::Io)?;
        std::fs::rename(&tmp, path).map_err(|_| StoreError::Io)?;
        Ok(())
    }

    fn read_envelope(&self, scope: Scope) -> PermissionsFile {
        let path = self.path_for(scope);
        if !path.exists() {
            return PermissionsFile::empty();
        }
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => return PermissionsFile::empty(),
        };
        // An empty file is treated as no rules (a fresh touch), not a decode
        // error, so a hand-created empty file does not brick startup.
        if bytes.is_empty() {
            return PermissionsFile::empty();
        }
        decode_envelope(&bytes, scope)
    }
}

/// The on-disk shape of a scope's permissions: a versioned envelope holding
/// the rule list and a directory-authorization list. Directories are 1:1 with
/// the sandbox fence's additional_dirs — a directory auth covers grep/glob/
/// read/edit/write, so persisting the list (not N per-tool rules) avoids
/// drift between two rule shapes that mean the same thing. The version field
/// is the migration anchor: a future revision that changes the shape reads it
/// to dispatch, instead of inferring v0 from a missing field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PermissionsFile {
    pub version: u32,
    pub rules: Vec<Rule>,
    pub directories: Vec<PathBuf>,
}

impl PermissionsFile {
    fn empty() -> Self {
        Self {
            version: 1,
            rules: Vec::new(),
            directories: Vec::new(),
        }
    }
}

/// Decode a scope file's envelope, stamping each rule's scope from the FILE it
/// was read from (the single source of truth for scope) — not the serialized
/// field. A legacy file written before the envelope existed is a bare JSON
/// array of rules (the prior shape); read it as an envelope with empty
/// directories so a hand-edited or pre-envelope file still loads. A corrupt
/// file is skipped (best-effort) so one bad scope cannot brick startup.
fn decode_envelope(bytes: &[u8], scope: Scope) -> PermissionsFile {
    if let Ok(mut env) = serde_json::from_slice::<PermissionsFile>(bytes) {
        for r in &mut env.rules {
            r.scope = scope;
        }
        return env;
    }
    // Legacy bare-array shape (pre-envelope): migrate in place.
    let rules = serde_json::from_slice::<Vec<Rule>>(bytes)
        .unwrap_or_default()
        .into_iter()
        .map(|mut r| {
            r.scope = scope;
            r
        })
        .collect();
    PermissionsFile {
        version: 1,
        rules,
        directories: Vec::new(),
    }
}

impl RuleStore for FileRuleStore {
    fn load(&self) -> Vec<Rule> {
        let mut all = Vec::new();
        // Stable order: user, then project, then local. Last-match-wins in
        // the evaluator means a later scope's rule shadows an earlier one
        // for the same action; this matches the intuitive layering (local
        // overrides project overrides user at decision time). read_scope
        // stamps each rule's scope from its file location (the single source
        // of truth), so a legacy/hand-edited rule lands in the right scope.
        for scope in [Scope::User, Scope::Project, Scope::Local] {
            all.extend(self.read_scope(scope));
        }
        all
    }

    fn add(&self, rule: &Rule) -> Result<(), StoreError> {
        // Honor the rule's own scope (its destination), not the store's
        // configured write_scope — a user-level always-allow carries its
        // scope on the rule and lands in the home file. the read_envelope call
        // preserves the directories list so a rule add cannot clobber a
        // prior directory authorization in the same scope file.
        let scope = rule.scope;
        let _guard = self.lock.lock().expect("rule store lock");
        let mut env = self.read_envelope(scope);
        // Idempotent no-op skip: if a Rule::same_as match already exists,
        // don't touch the file (avoids needless rename churn racing a
        // concurrent reader — same reasoning as remove's no-op skip).
        // Rule::same_as is the shared identity judgment, so add and remove
        // agree on "same rule".
        if env.rules.iter().all(|r| !r.same_as(rule)) {
            env.rules.push(rule.clone());
            self.write_envelope_unlocked(scope, &env)?;
        } else {
            tracing::debug!(
                action = %rule.action,
                scope = ?rule.scope,
                "[permission] store add: duplicate rule skipped (no write)"
            );
        }
        Ok(())
    }

    fn remove(&self, rule: &Rule) -> Result<(), StoreError> {
        let scope = rule.scope;
        let _guard = self.lock.lock().expect("rule store lock");
        let mut env = self.read_envelope(scope);
        let before = env.rules.len();
        // Rule::same_as identity match: case-insensitive action + exact
        // content + effect + scope (the judgment add uses, so the pair stays
        // symmetric). Deletes every matching rule, not just one — a
        // pre-existing dup file collapses to zero here (dedup prevents new
        // dups, not existing ones). Action-only matching would wipe same-
        // action siblings (bash npm:* + bash git:* share action="bash");
        // content + effect + scope prevent that.
        env.rules.retain(|r| !r.same_as(rule));
        // Skip the write when nothing matched: a no-op remove should not
        // touch the file (avoids a needless rename churn that could race a
        // concurrent reader).
        if env.rules.len() == before {
            tracing::debug!(
                action = %rule.action,
                scope = ?rule.scope,
                "[permission] store remove: no matching rule (no write)"
            );
            return Ok(());
        }
        self.write_envelope_unlocked(scope, &env)
    }

    fn clear(&self) -> Result<(), StoreError> {
        self.write_envelope_unlocked(self.write_scope, &PermissionsFile::empty())
    }

    fn load_directories(&self) -> Vec<PathBuf> {
        let mut all = Vec::new();
        for scope in [Scope::User, Scope::Project, Scope::Local] {
            all.extend(self.read_envelope(scope).directories);
        }
        all
    }

    fn add_directory(&self, dir: &Path, scope: Scope) -> Result<(), StoreError> {
        let _guard = self.lock.lock().expect("rule store lock");
        let mut env = self.read_envelope(scope);
        let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if env.directories.iter().all(|d| d != &canonical) {
            env.directories.push(canonical);
            self.write_envelope_unlocked(scope, &env)?;
        }
        Ok(())
    }

    fn remove_directory(&self, dir: &Path, scope: Scope) -> Result<(), StoreError> {
        let _guard = self.lock.lock().expect("rule store lock");
        let mut env = self.read_envelope(scope);
        let canonical = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        let before = env.directories.len();
        env.directories.retain(|d| d != &canonical);
        if env.directories.len() == before {
            return Ok(());
        }
        self.write_envelope_unlocked(scope, &env)
    }
}

/// Resolve the user home directory. Falls back to the project-local dot
/// directory when HOME is unset (a sandboxed environment) so the user scope
/// still has a writable path; the project scope is the authoritative one in
/// that case anyway.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
