//! Tests for this module.

use super::*;
use houyicoder_async::PFut;
use houyicoder_context::{ExecConfig, ExecResult, SandboxError};
use houyicoder_permission::{FileRuleStore, Scope};
use houyicoder_sandbox::PlatformSession;
use std::env::temp_dir;
use std::fs;
#[cfg(windows)]
use std::fs::canonicalize;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::process::Command;
use std::process::id;

/// Never default_paths: that would write the developer's real home.
fn temp_store(root: &Path) -> Arc<dyn RuleStore> {
    Arc::new(FileRuleStore::new(
        root.join("user.json"),
        root.join("project.json"),
        root.join("local.json"),
    ))
}

fn temp_root(tag: &str) -> PathBuf {
    let root = temp_dir().join(format!("{tag}-{}", id()));
    drop(fs::remove_dir_all(&root));
    fs::create_dir_all(&root).expect("mkdir root");
    root
}

/// The grant is durable but the fence is in-memory and starts empty, so
/// startup has to carry it across. Without that the store still lists the
/// directory while the fence does not, and the tool refuses a path the user
/// already approved.
#[test]
fn test_startup_restores_fence() {
    let root = temp_root("houyi-restore");
    let store = temp_store(&root);
    let target = root.join("authorized-dir");
    fs::create_dir_all(&target).expect("mkdir target");
    store
        .add_directory(&target, Scope::Project)
        .expect("add_directory");
    let read_target = root.join("read-dir");
    fs::create_dir_all(&read_target).expect("mkdir read target");
    store
        .add_read_directory(&read_target, Scope::Project)
        .expect("add_read_directory");
    let canonical = dunce::canonicalize(&target).expect("canonicalize target");
    let canonical_read = dunce::canonicalize(&read_target).expect("canonicalize read target");

    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    assert!(
        session.working_dirs().is_empty(),
        "fence starts empty before the restore"
    );

    restore_directory_grants(session.as_ref(), store.as_ref()).expect("restore");
    let dirs = session.working_dirs();
    assert!(
        dirs.iter().any(|d| Path::new(d.as_str()) == canonical),
        "the persisted directory must be back in the fence: {dirs:?}"
    );
    assert!(
        session
            .reading_dirs()
            .iter()
            .any(|dir| Path::new(dir) == canonical_read),
        "the read-only grant must restore separately"
    );
    assert!(
        session
            .writing_dirs()
            .iter()
            .all(|dir| Path::new(dir) != canonical_read),
        "restore must not upgrade read-only access"
    );
    fs::remove_dir_all(&root).ok();
}

/// A directory deleted since it was persisted must not brick startup. The
/// stale entry is skipped and the still-valid one re-attaches, so one bad
/// entry in the store cannot cost the user every other grant.
#[test]
fn test_startup_skips_stale_dir() {
    let root = temp_root("houyi-restore-stale");
    let store = temp_store(&root);
    let target = root.join("authorized-dir");
    fs::create_dir_all(&target).expect("mkdir target");
    store
        .add_directory(&target, Scope::Project)
        .expect("add_directory");
    let canonical = dunce::canonicalize(&target).expect("canonicalize target");
    // Never created, so its re-attach fails.
    store
        .add_directory(&root.join("stale-deleted"), Scope::Project)
        .expect("add stale dir");

    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    restore_directory_grants(session.as_ref(), store.as_ref()).expect("restore");

    let dirs = session.working_dirs();
    assert!(
        dirs.iter().any(|d| Path::new(d.as_str()) == canonical),
        "the valid directory still re-attaches: {dirs:?}"
    );
    assert!(
        !dirs.iter().any(|d| d.contains("stale-deleted")),
        "the stale directory must not re-attach: {dirs:?}"
    );
    fs::remove_dir_all(&root).ok();
}

/// A stored grant whose path now leads somewhere else must not re-attach:
/// the entry names the directory it was saved for, and a link retargeted
/// after the fact would otherwise hand over whatever that path points at
/// today.
#[cfg(unix)]
#[test]
fn test_retargeted_link_not_restored() {
    let root = temp_root("houyi-restore-retarget");
    let store = temp_store(&root);
    let target = root.join("authorized-dir");
    fs::create_dir_all(&target).expect("mkdir target");
    store
        .add_directory(&target, Scope::Project)
        .expect("add_directory");
    let granted = dunce::canonicalize(&target).expect("canonicalize target");
    // Replace the granted directory with a link to one the user never named.
    let elsewhere = root.join("elsewhere");
    fs::create_dir_all(&elsewhere).expect("mkdir elsewhere");
    let elsewhere = dunce::canonicalize(&elsewhere).expect("canonicalize elsewhere");
    fs::remove_dir_all(&target).expect("remove granted dir");
    symlink(&elsewhere, &target).expect("link target to elsewhere");

    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    restore_directory_grants(session.as_ref(), store.as_ref()).expect("restore");

    let dirs = session.working_dirs();
    assert!(
        !dirs.iter().any(|d| Path::new(d.as_str()) == granted),
        "the grant must not re-attach once its path leads elsewhere: {dirs:?}"
    );
    assert!(
        !dirs.iter().any(|d| Path::new(d.as_str()) == elsewhere),
        "the directory the link now names was never granted: {dirs:?}"
    );
    fs::remove_dir_all(&root).ok();
}

/// Init a git repo with one commit, and link a worktree under it. Returns
/// None when git is unavailable or any step fails, so the caller skips
/// rather than failing on a missing tool.
#[expect(clippy::disallowed_methods, reason = "test fixture, not model-driven")]
fn git_repo_with_worktree(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).ok()?;
    let git = |args: &[&str], cwd: &Path| {
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    git(&["init"], &repo)?;
    git(&["config", "user.email", "t@t"], &repo)?;
    git(&["config", "user.name", "t"], &repo)?;
    fs::write(repo.join("f.txt"), b"x").ok()?;
    git(&["add", "."], &repo)?;
    git(&["commit", "-m", "init"], &repo)?;
    let wt = root.join("wt");
    git(&["worktree", "add", wt.to_str()?], &repo)?;
    Some((repo, wt))
}

/// A session whose workspace IS a linked worktree must get the main repo
/// git dir into the fence. That dir holds the worktree's index, plus the
/// objects and refs a commit writes, and it sits outside the worktree, so
/// without the allow-back git cannot write at all -- add, commit and stash
/// all abort on index.lock. The enter-worktree path allow-backs it when it
/// narrows; a session that merely STARTS inside an existing worktree never
/// goes through that path.
#[test]
fn test_worktree_attaches_git_common() {
    let root = temp_root("houyi-wt-gitcommon");
    let Some((repo, wt)) = git_repo_with_worktree(&root) else {
        fs::remove_dir_all(&root).ok();
        return;
    };
    let wt_canon = dunce::canonicalize(&wt).expect("canonicalize worktree");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&wt_canon).expect("sandbox"));
    assert!(
        session.working_dirs().is_empty(),
        "fence starts with no extra dirs"
    );

    attach_git_common_dir(session.as_ref(), &wt_canon);

    let main_git = dunce::canonicalize(repo.join(".git")).expect("canonicalize main .git");
    let dirs = session.working_dirs();
    assert!(
        dirs.iter().any(|d| Path::new(d.as_str()) == main_git),
        "the main repo git dir must be in the fence so git can write from the worktree: {dirs:?}"
    );
    fs::remove_dir_all(&root).ok();
}

/// A main repo needs no allow-back: its git dir is inside the workspace,
/// already covered. Adding it anyway would widen nothing but would show up
/// as a working dir in /permissions, telling the user the fence was
/// extended when it was not.
#[test]
fn test_main_repo_attaches_nothing() {
    let root = temp_root("houyi-main-gitcommon");
    let Some((repo, _wt)) = git_repo_with_worktree(&root) else {
        fs::remove_dir_all(&root).ok();
        return;
    };
    let repo_canon = dunce::canonicalize(&repo).expect("canonicalize repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo_canon).expect("sandbox"));

    attach_git_common_dir(session.as_ref(), &repo_canon);

    assert!(
        session.working_dirs().is_empty(),
        "a main repo's git dir is already inside the workspace: {:?}",
        session.working_dirs()
    );
    fs::remove_dir_all(&root).ok();
}

/// Production hands attach a std-canonicalized workspace, which on
/// Windows carries the verbatim prefix, while the common dir resolves to
/// the plain form. Without normalizing the workspace side, the main
/// repo's own git dir fails the containment check and gets attached as a
/// runtime grant, showing a fence extension that widened nothing.
#[cfg(windows)]
#[test]
fn test_attach_normalizes_verbatim() {
    let root = temp_root("houyi-verbatim");
    let Some((repo, _wt)) = git_repo_with_worktree(&root) else {
        fs::remove_dir_all(&root).ok();
        return;
    };
    let ws = canonicalize(&repo).expect("canonicalize repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&ws).expect("sandbox"));

    attach_git_common_dir(session.as_ref(), &ws);

    assert!(
        session.working_dirs().is_empty(),
        "a verbatim workspace must still contain its own git dir: {:?}",
        session.working_dirs()
    );
    fs::remove_dir_all(&root).ok();
}

/// The positive counterpart: a linked worktree passed in verbatim form
/// must still get the main repo git dir attached. The probe runs after
/// normalization, so this reddens if the verbatim path ever reaches git
/// for Windows (which is widely reported to reject it, making the
/// allow-back a silent no-op) instead of only checking the negative
/// containment case.
#[cfg(windows)]
#[test]
fn test_attach_verbatim_worktree() {
    let root = temp_root("houyi-verbatim-wt");
    let Some((repo, wt)) = git_repo_with_worktree(&root) else {
        fs::remove_dir_all(&root).ok();
        return;
    };
    let ws = canonicalize(&wt).expect("canonicalize worktree");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&ws).expect("sandbox"));

    attach_git_common_dir(session.as_ref(), &ws);

    let main_git = dunce::canonicalize(repo.join(".git")).expect("canonicalize main .git");
    let dirs = session.working_dirs();
    assert!(
        dirs.iter().any(|d| Path::new(d.as_str()) == main_git),
        "a verbatim linked worktree must still attach the main git dir: {dirs:?}"
    );
    fs::remove_dir_all(&root).ok();
}

/// The gate asks through Containment instead of holding the session, so the
/// query must report the bounds the session enforces: workspace root plus
/// runtime-added dirs. Drop either and the gate sees a smaller fence than
/// the kernel does, refusing paths that are in fact allowed.
#[test]
fn test_containment_reports_bounds() {
    let root = temp_root("adapter-bound");
    let extra = root.join("extra");
    fs::create_dir_all(&extra).expect("mkdir extra");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&root).expect("sandbox"));
    session
        .add_working_dir(&extra.to_string_lossy())
        .expect("widen the fence");

    let adapter = ContainmentAdapter(session);
    assert_eq!(
        adapter.boundary_root().map(|p| p.to_path_buf()),
        Some(dunce::canonicalize(&root).expect("canonicalize root")),
        "the root the session enforces must be the root the gate sees"
    );
    let dirs = adapter.boundary_dirs();
    let widened = dunce::canonicalize(&extra).expect("canonicalize extra");
    assert!(
        dirs.iter().any(|d| d == &widened),
        "a runtime-added dir must be in the bounds the gate sees: {dirs:?}"
    );
    fs::remove_dir_all(&root).ok();
}

/// A session that reports a capability and a revision of its own, so the
/// pass-through can be read off without depending on what the host platform
/// enforces.
struct CapableSession;

impl Containment for CapableSession {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: Vec::new(),
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn capability(&self) -> FenceCapability {
        FenceCapability {
            writes: true,
            reads: false,
            network: true,
        }
    }
    fn capability_revision(&self) -> u64 {
        42
    }
}

impl SandboxSession for CapableSession {
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(Path::new("/ws"))
    }
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
    fn as_containment(&self) -> Option<&dyn Containment> {
        Some(self)
    }
}

/// A backend whose fence grants one shared temp root on top of its scratch
/// directory, so the adapter's report of that root can be read without
/// depending on what the host platform's fence happens to grant.
struct SharedTempSession;

impl SandboxSession for SharedTempSession {
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(Path::new("/ws"))
    }
    fn scratch_dir(&self) -> Option<PathBuf> {
        Some(PathBuf::from("/ws/scratch"))
    }
    fn shared_temp_roots(&self) -> Vec<PathBuf> {
        vec![PathBuf::from("/shared-temp")]
    }
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
}

/// A backend that grants nothing shared, for the negative half below.
struct NothingShared;

impl SandboxSession for NothingShared {
    fn workspace_root(&self) -> Arc<Path> {
        Arc::from(Path::new("/ws"))
    }
    fn exec_with_config(
        &self,
        _command: &str,
        _config: ExecConfig,
    ) -> PFut<'_, Result<ExecResult, SandboxError>> {
        Box::pin(async {
            Ok(ExecResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: Some(0),
            })
        })
    }
}

/// Which shared temp roots count as inside the fence is the backend's answer.
///
/// A fence that opens the system temp root leaves a write there inside it, so
/// the gate must not ask. A path-based ruleset that grants only the workspace
/// and the scratch directory leaves the same write outside, so the gate must
/// ask. Naming the roots in this layer instead made the gate stop asking on
/// the second kind of backend, and the kernel then refused a call the gate had
/// already approved.
#[test]
fn test_bounds_follow_temp_roots() {
    let granting = ContainmentAdapter(Arc::new(SharedTempSession));
    let dirs = granting.boundary_dirs();
    assert!(
        dirs.contains(&PathBuf::from("/shared-temp")),
        "a root the backend grants is inside the fence: {dirs:?}"
    );
    assert!(
        dirs.contains(&PathBuf::from("/ws/scratch")),
        "and so is the session scratch dir: {dirs:?}"
    );

    let withholding = ContainmentAdapter(Arc::new(NothingShared));
    assert!(
        withholding.boundary_dirs().is_empty(),
        "a backend that grants nothing shared adds nothing: {:?}",
        withholding.boundary_dirs()
    );
}

/// The gate reads the capability and its revision through the adapter, so a
/// fallback that reported a different fence than the session enforces would
/// either relax what the session never earned or ask against a fence that is
/// in place. The axes and the counter both travel unchanged.
#[test]
fn test_adapter_reports_capability() {
    let adapter = ContainmentAdapter(Arc::new(CapableSession));
    assert_eq!(
        adapter.capability(),
        FenceCapability {
            writes: true,
            reads: false,
            network: true
        }
    );
    assert_eq!(adapter.capability_revision(), 42);
}

/// A fence that cannot widen at runtime refuses both restores, and each
/// refusal is counted rather than ending startup: one grant this backend
/// cannot honour must not cost the user the rest of the session.
#[test]
fn test_refused_grant_keeps_starting() {
    let root = temp_root("houyi-restore-refused");
    let store = temp_store(&root);
    let target = root.join("authorized-dir");
    fs::create_dir_all(&target).expect("mkdir target");
    store
        .add_directory(&target, Scope::Project)
        .expect("add_directory");
    // A separate read-only directory: the store folds a read grant into an
    // existing write grant, so reusing the same path would never reach the
    // read re-attach.
    let read_target = root.join("read-dir");
    fs::create_dir_all(&read_target).expect("mkdir read target");
    store
        .add_read_directory(&read_target, Scope::Project)
        .expect("add_read_directory");
    let session = CapableSession;

    restore_directory_grants(&session, store.as_ref()).expect("restore");

    assert!(
        session.working_dirs().is_empty(),
        "a fence that refuses the grant reports nothing granted: {:?}",
        session.working_dirs()
    );
    fs::remove_dir_all(&root).ok();
}

/// A store that cannot be read fails the restore rather than reporting no
/// grants. The fence starts empty either way, but the caller has to know the
/// file it must repair, and a silent empty answer would look like a user who
/// granted nothing.
#[test]
fn test_corrupt_store_fails_restore() {
    let root = temp_root("houyi-restore-corrupt");
    let store = temp_store(&root);
    fs::write(root.join("project.json"), b"{ not a permissions file")
        .expect("write a corrupt scope file");
    let session = CapableSession;

    let error = restore_directory_grants(&session, store.as_ref())
        .expect_err("a store that cannot be read must fail the restore");

    assert!(
        error.to_string().contains("project.json"),
        "the failure names the file to repair: {error}"
    );
    assert!(
        session.working_dirs().is_empty(),
        "the fence starts without listings it could not verify: {:?}",
        session.working_dirs()
    );
    fs::remove_dir_all(&root).ok();
}

/// A store that cannot be read still yields a usable session, but no grant it
/// could not verify is re-attached: a listing the user may have revoked stays
/// out of the fence, and the gate names the file the restore failed on.
#[test]
fn test_corrupt_store_starts_bare() {
    let root = temp_root("houyi-fences-corrupt");
    let store = temp_store(&root);
    let target = root.join("authorized-dir");
    fs::create_dir_all(&target).expect("mkdir target");
    store
        .add_directory(&target, Scope::Project)
        .expect("record a grant");
    fs::write(root.join("project.json"), b"{ not a permissions file")
        .expect("corrupt the scope file after the grant was written");
    let canonical = dunce::canonicalize(&target).expect("canonicalize target");

    let fences = build_session_fences(Some(root.to_string_lossy().into_owned()), Some(store));

    let session = fences
        .sandbox_session
        .expect("the fence still starts a session");
    let dirs = session.working_dirs();
    assert!(
        dirs.iter().all(|dir| Path::new(dir.as_str()) != canonical),
        "a grant that cannot be verified is not re-attached: {dirs:?}"
    );
    assert!(
        fences.gate.store_failure().is_some(),
        "the gate reports the file the fence could not restore from"
    );
    fs::remove_dir_all(&root).ok();
}

/// A skill discovered under the user's home is read through the same fence as
/// any other file, so the trees holding them are granted read-only at start.
/// Only trees that exist are granted: naming a missing directory would make
/// the fence refuse an install the machine never had.
#[test]
fn test_user_skill_roots_granted() {
    let root = temp_root("houyi-skills");
    let home = root.join("home");
    let claude = home.join(".claude").join("skills");
    fs::create_dir_all(&claude).expect("mkdir claude skills");
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    let session: Arc<dyn SandboxSession> =
        Arc::new(PlatformSession::new_in_cwd(&repo).expect("sandbox"));
    assert!(
        session.reading_dirs().is_empty(),
        "fence starts empty before the grant"
    );

    attach_user_skill_roots_in(session.as_ref(), &home);

    let canonical = dunce::canonicalize(&claude).expect("canonicalize skills");
    assert!(
        session
            .reading_dirs()
            .iter()
            .any(|dir| Path::new(dir) == canonical),
        "the existing skill tree is readable: {:?}",
        session.reading_dirs()
    );
    assert_eq!(
        session.reading_dirs().len(),
        1,
        "the families with no tree are not named: {:?}",
        session.reading_dirs()
    );
    fs::remove_dir_all(&root).ok();
}
