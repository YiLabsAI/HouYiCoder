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
use std::env::current_dir;
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::create_dir_all;
use std::fs::read;
use std::fs::remove_file;
use std::fs::rename;
use std::fs::write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::id;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};

use crate::rule::Rule;
use houyicoder_api::path::{expand_home, home_directory};

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

impl Display for StoreError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io => f.write_str("rule store I/O failure"),
            Self::Decode => f.write_str("rule store decode failure"),
            Self::Encode => f.write_str("rule store encode failure"),
        }
    }
}

impl Error for StoreError {}

/// The persistence seam for durable rules. Object-safe so the gate holds a
/// single shared dispatcher. The in-memory fast path skips this entirely; an
/// impl is attached when an always-allow verdict should survive a restart.
///
/// FileRuleStore serializes reads and writes across processes with an
/// advisory lock, so several terminals may share one scope without losing a
/// rule. Another impl may still be single-writer: the trait promises only
/// that a caller need not hold an external lock.
///
/// load returns the union of all scopes. add and remove write the scope the
/// rule itself carries; clear empties the store's configured write scope and
/// leaves the other scopes untouched.
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

    /// Read the union of read-only directory authorizations.
    fn load_read_directories(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Append a directory authorization to the given scope's envelope. Atomic
    /// (tmp + rename) like add. Default no-op so non-file impls stay unchanged.
    fn add_directory(&self, _dir: &Path, _scope: Scope) -> Result<(), StoreError> {
        Ok(())
    }

    /// Append a read-only directory authorization.
    fn add_read_directory(&self, _dir: &Path, _scope: Scope) -> Result<(), StoreError> {
        Ok(())
    }

    /// Remove a directory authorization from the given scope. No-op when
    /// absent. Default no-op.
    fn remove_directory(&self, _dir: &Path, _scope: Scope) -> Result<(), StoreError> {
        Ok(())
    }
}

/// Per-process counter separating the temp names one process writes inside
/// the same instant; the pid separates processes.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// One cross-process claim on a scope file, released when dropped. The lock
/// file lives beside the scope file, so stores writing different scopes never
/// serialize on each other, and the kernel drops the claim when the holder
/// exits, so a dead writer never leaves the store locked.
struct ScopeLock {
    /// Held open for as long as the kernel lock is held.
    _file: File,
}

impl ScopeLock {
    /// Claim the lock of the scope file at path, waiting for any current
    /// holder. A lock that cannot be opened fails the write, because
    /// continuing without it would let two processes clobber each other's
    /// envelope.
    fn acquire(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            create_dir_all(parent).map_err(|_| StoreError::Io)?;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("permissions.json");
        let mut open = OpenOptions::new();
        open.create(true).write(true).truncate(false);
        #[cfg(unix)]
        open.mode(0o600);
        let file = open
            .open(path.with_file_name(format!(".{name}.lock")))
            .map_err(|_| StoreError::Io)?;
        file.lock().map_err(|_| StoreError::Io)?;
        Ok(Self { _file: file })
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
        let cwd = current_dir().unwrap_or_else(|_| PathBuf::from("."));
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

    /// Compose one read-modify-write of a scope file. The in-process lock and
    /// the scope's cross-process lock are held for the whole cycle, and the
    /// edit closure reports whether it changed anything so an unchanged call
    /// leaves the file untouched. Serializing the read and the write inside
    /// one call is what keeps two writers from each landing a whole record
    /// derived from the state they read.
    fn update_envelope<F>(&self, scope: Scope, edit: F) -> Result<(), StoreError>
    where
        F: FnOnce(&mut PermissionsFile) -> bool,
    {
        let _guard = self.lock.lock().expect("rule store lock");
        let path = self.path_for(scope);
        let _scope_lock = ScopeLock::acquire(path)?;
        let mut env = self.read_envelope(scope);
        if !edit(&mut env) {
            return Ok(());
        }
        self.write_envelope(path, &env)
    }

    /// Replace a scope file under the caller-held scope lock. The bytes land
    /// through a temp file named for this writer, then rename over the target,
    /// so a reader never sees a half-written envelope and two processes never
    /// target one temp file.
    fn write_envelope(&self, path: &Path, env: &PermissionsFile) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            create_dir_all(parent).map_err(|_| StoreError::Io)?;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("permissions.json");
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_file_name(format!(".{name}.{}.{seq}.tmp", id()));
        let mut persisted = env.clone();
        persisted.version = 2;
        let bytes = serde_json::to_vec(&persisted).map_err(|_| StoreError::Encode)?;
        if write(&tmp, bytes).is_err() {
            drop(remove_file(&tmp));
            return Err(StoreError::Io);
        }
        if rename(&tmp, path).is_err() {
            drop(remove_file(&tmp));
            return Err(StoreError::Io);
        }
        Ok(())
    }

    fn read_envelope(&self, scope: Scope) -> PermissionsFile {
        let path = self.path_for(scope);
        if !path.exists() {
            return PermissionsFile::empty();
        }
        let bytes = match read(path) {
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

/// Versioned permissions envelope with rules and separate read-only and
/// read-write directory grants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PermissionsFile {
    pub version: u32,
    pub rules: Vec<Rule>,
    pub directories: Vec<PathBuf>,
    #[serde(default)]
    pub read_directories: Vec<PathBuf>,
}

impl PermissionsFile {
    fn empty() -> Self {
        Self {
            version: 2,
            rules: Vec::new(),
            directories: Vec::new(),
            read_directories: Vec::new(),
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
        // Entries an older writer persisted may carry the Windows verbatim
        // prefix; reduce them to the plain drive-letter form so removal
        // matches them and a fence comparison holds. A no-op elsewhere.
        for d in env
            .directories
            .iter_mut()
            .chain(env.read_directories.iter_mut())
        {
            *d = dunce::simplified(d).to_path_buf();
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
        version: 2,
        rules,
        directories: Vec::new(),
        read_directories: Vec::new(),
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
        self.update_envelope(scope, |env| {
            // A rule already in the file leaves it untouched, which avoids
            // needless rename churn. The identity judgment is the one remove
            // uses, so the two operations agree on what counts as the same
            // rule.
            if env.rules.iter().any(|r| r.same_as(rule)) {
                tracing::debug!(
                    action = %rule.action,
                    scope = ?rule.scope,
                    "[permission] store add: duplicate rule skipped (no write)"
                );
                return false;
            }
            env.rules.push(rule.clone());
            true
        })
    }

    fn remove(&self, rule: &Rule) -> Result<(), StoreError> {
        let scope = rule.scope;
        self.update_envelope(scope, |env| {
            let before = env.rules.len();
            // Remove by the identity judgment add uses, so the two agree on
            // what counts as the same rule. Every match goes, which collapses
            // a pre-existing duplicate file to nothing. Matching on the action
            // alone would also delete sibling rules that share that action.
            env.rules.retain(|r| !r.same_as(rule));
            // A no-op remove leaves the file untouched.
            if env.rules.len() == before {
                tracing::debug!(
                    action = %rule.action,
                    scope = ?rule.scope,
                    "[permission] store remove: no matching rule (no write)"
                );
                return false;
            }
            true
        })
    }

    fn clear(&self) -> Result<(), StoreError> {
        // Empty the rules but keep the envelope's other members: the
        // directory authorizations are the user's sandbox grants, and
        // overwriting the whole file would drop them. Running as the same
        // composed edit add and remove use keeps a clear from racing a
        // concurrent add and losing its rule.
        self.update_envelope(self.write_scope, |env| {
            if env.rules.is_empty() {
                return false;
            }
            env.rules.clear();
            true
        })
    }

    fn load_directories(&self) -> Vec<PathBuf> {
        let mut all = Vec::new();
        for scope in [Scope::User, Scope::Project, Scope::Local] {
            all.extend(self.read_envelope(scope).directories);
        }
        all
    }

    fn add_directory(&self, dir: &Path, scope: Scope) -> Result<(), StoreError> {
        let canonical = canonical_dir(dir);
        self.update_envelope(scope, |env| {
            if env.directories.iter().any(|path| path == &canonical) {
                return false;
            }
            // A write grant subsumes a pending read grant for the same
            // directory, so the two lists never carry the same path.
            env.read_directories.retain(|path| path != &canonical);
            env.directories.push(canonical.clone());
            true
        })
    }

    fn load_read_directories(&self) -> Vec<PathBuf> {
        let mut all = Vec::new();
        for scope in [Scope::User, Scope::Project, Scope::Local] {
            all.extend(self.read_envelope(scope).read_directories);
        }
        all
    }

    fn add_read_directory(&self, dir: &Path, scope: Scope) -> Result<(), StoreError> {
        let canonical = canonical_dir(dir);
        self.update_envelope(scope, |env| {
            if env.directories.iter().any(|path| path == &canonical) {
                return false;
            }
            if env.read_directories.iter().any(|path| path == &canonical) {
                return false;
            }
            env.read_directories.push(canonical.clone());
            true
        })
    }

    fn remove_directory(&self, dir: &Path, scope: Scope) -> Result<(), StoreError> {
        let canonical = canonical_dir(dir);
        self.update_envelope(scope, |env| {
            let before = env.directories.len() + env.read_directories.len();
            env.directories.retain(|path| !same_dir(path, &canonical));
            env.read_directories
                .retain(|path| !same_dir(path, &canonical));
            env.directories.len() + env.read_directories.len() != before
        })
    }
}

/// Resolve the user home directory. Falls back to the project-local dot
/// directory when the platform reports no home (a sandboxed environment) so the
/// user scope still has a writable path; the project scope is the authoritative
/// one in that case anyway.
fn home_dir() -> PathBuf {
    home_directory().unwrap_or_else(|| PathBuf::from("."))
}

/// Whether two spellings name the same granted directory. Windows compares
/// paths without regard to case, so a removal typed in another case still
/// matches the record; elsewhere two spellings that differ in case name two
/// directories, and clearing the wrong grant would revoke access the user did
/// not ask to lose.
fn same_dir(stored: &Path, target: &Path) -> bool {
    if stored == target {
        return true;
    }
    if !cfg!(windows) {
        return false;
    }
    stored
        .to_string_lossy()
        .eq_ignore_ascii_case(&target.to_string_lossy())
}

/// The directory a typed path names. A directory grant is compared on a later
/// removal against the path the caller reads back from the file system, so
/// the stored spelling has to be the expanded one: a tilde kept as typed
/// names a directory the removal can never match.
fn canonical_dir(dir: &Path) -> PathBuf {
    let expanded = expand_home(dir, home_directory().as_deref());
    if let Ok(canonical) = dunce::canonicalize(&expanded) {
        return canonical;
    }
    // A directory removed from the file system still has to match the record
    // that granted it, so resolve the parent and keep the leaf as written
    // rather than comparing a possibly unnormalized spelling.
    match (expanded.parent(), expanded.file_name()) {
        (Some(parent), Some(leaf)) => match dunce::canonicalize(parent) {
            Ok(parent) => parent.join(leaf),
            Err(_) => expanded,
        },
        _ => expanded,
    }
}

#[cfg(test)]
mod tests;
