//! What the kernel actually permits under a rendered seatbelt profile. Every
//! test here renders the real profile against the real home directory and runs
//! a command through sandbox-exec, so the assertion is the kernel's verdict,
//! not the contents of the profile string.
//!
//! That distinction is the whole reason this file exists, and it is what
//! separates it from the string-level tests beside the profile builders: a
//! rule can be present and still be dead. The system-etc allow segment was
//! emitted correctly for a long time while the kernel refused every read it
//! named, because the broad deny of the etc subpath also covered the etc
//! symlink node, so path resolution failed before the leaf rule was ever
//! evaluated. The string assertions were green throughout. Only a real spawn
//! catches that class of bug.
//!
//! Classification: integration, not unit. These drive a real external
//! subsystem, which the unit suite is required never to do, so the commit gate
//! never runs them and the integration suite does: make test integration
//! compiles the whole tests tree and runs the tests that carry no ignore. The
//! sandbox suite holds this file as a module, so the file declares no target
//! of its own. Everything is compiled out where there is no seatbelt.
//!
//! Human-readable report, useful when debugging a profile change, run from
//! this crate's directory:
//!   cargo test --test sandbox_all seatbelt_kernel:: -- --nocapture
//! Set HOUYI_PROFILE_DUMP=1 to also print the full rendered profile.

// These probes must invoke sandbox-exec directly: the thing under test IS the
// kernel's verdict on a rendered profile, so routing the spawn through the
// launcher would put the subject behind one of its own consumers. Same
// reasoning as the seatbelt test scaffolding. Production code still routes
// every spawn through the launcher.
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "macos")]

use houyicoder_api::sandbox::NetworkPolicy;
use houyicoder_sandbox::ProfileSpec;
use std::env::temp_dir;
use std::env::var;
use std::fs::create_dir_all;
use std::fs::remove_dir;
use std::fs::remove_dir_all;
use std::fs::remove_file;
use std::fs::write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::id;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use std::time::Instant;

#[path = "seatbelt_kernel/network.rs"]
mod network;

/// A live fence: a throwaway workspace, a sibling per-session temporary
/// directory, and the profile rendered for them. Both directories are removed
/// on drop so a failing assertion leaves nothing behind.
#[cfg(target_os = "macos")]
struct Fence {
    workspace: PathBuf,
    tmpdir: PathBuf,
    profile: String,
    home: String,
}

/// One probe result. A denial arrives as a non-zero exit whose stderr carries
/// the kernel's operation-not-permitted text, so both halves are kept: the
/// code decides allowed versus denied, and the text distinguishes a denial
/// from an ordinary failure such as a missing file.
#[cfg(target_os = "macos")]
struct Outcome {
    code: Option<i32>,
    stderr: String,
    elapsed: Duration,
}

#[cfg(target_os = "macos")]
impl Outcome {
    /// True when the command completed cleanly. Every fence denial fails.
    fn allowed(&self) -> bool {
        self.code == Some(0)
    }

    /// True when the failure came from the fence rather than from the command.
    fn denied_by_fence(&self) -> bool {
        self.stderr.contains("not permitted")
    }

    /// Human-readable verdict for the printed report.
    fn verdict(&self) -> &'static str {
        if self.allowed() { "ALLOWED" } else { "DENIED" }
    }

    /// Why the attempt failed, phrased for an assertion message. Reports the
    /// elapsed time because that is the evidence a network verdict rests on.
    fn detail(&self) -> String {
        format!(
            "exit {:?} after {}ms, stderr: {}",
            self.code,
            self.elapsed.as_millis(),
            if self.stderr.is_empty() {
                "(empty)"
            } else {
                &self.stderr
            }
        )
    }
}

#[cfg(target_os = "macos")]
impl Fence {
    /// Render the real profile against a fresh throwaway workspace. The home
    /// directory is the real one on purpose: the home git config rules are
    /// exactly what several of these probes verify.
    fn new(label: &str) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let home = var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
        // Canonicalized because the session does the same before rendering, and
        // the kernel matches rules against the resolved path. On this platform
        // the temp directory is reached through a symlink, so the unresolved
        // form names a path the kernel never sees: every allow under it would
        // silently fail to match, and every write probe would then be denied for
        // that reason rather than by the rule it means to exercise.
        let root = dunce::canonicalize(temp_dir()).expect("canonicalize temp dir");
        let tag = format!(
            "houyi-fence-{label}-{}-{}",
            id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let workspace = root.join(&tag);
        let tmpdir = root.join(format!("{tag}-tmp"));
        create_dir_all(&workspace).expect("create probe workspace");
        create_dir_all(&tmpdir).expect("create probe tmpdir");
        let profile = houyicoder_sandbox::render(&ProfileSpec::new(
            &workspace,
            &tmpdir.to_string_lossy(),
            &home,
            &tag,
        ));
        if var("HOUYI_PROFILE_DUMP").is_ok() {
            println!("{profile}");
        }
        Self {
            workspace,
            tmpdir,
            profile,
            home,
        }
    }

    /// Render the real profile with an explicit network posture. Separate from
    /// new so the filesystem probes keep the contained default they assert
    /// against, and so a network probe states its posture at the call site.
    fn with_network(label: &str, network: NetworkPolicy) -> Self {
        let mut f = Self::new(label);
        f.profile = houyicoder_sandbox::render(
            &ProfileSpec::new(&f.workspace, &f.tmpdir.to_string_lossy(), &f.home, label)
                .with_network(network),
        );
        f
    }

    /// Run one shell command inside the fence and report the verdict.
    fn probe(&self, label: &str, cmd: &str) -> Outcome {
        let started = Instant::now();
        let out = Command::new("sandbox-exec")
            .arg("-p")
            .arg(&self.profile)
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.workspace)
            .env("TMPDIR", &self.tmpdir)
            .output()
            .expect("spawn sandbox-exec");
        let o = Outcome {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            elapsed: started.elapsed(),
        };
        println!(
            "[{label}] {} in {}ms :: {} :: {}",
            o.verdict(),
            o.elapsed.as_millis(),
            cmd,
            o.stderr
        );
        o
    }
}

#[cfg(target_os = "macos")]
impl Drop for Fence {
    fn drop(&mut self) {
        remove_dir_all(&self.workspace).ok();
        remove_dir_all(&self.tmpdir).ok();
    }
}

/// The fence must actually be enforcing, or every other verdict in this file
/// is meaningless. The system password file is outside the workspace and is
/// named by no allow rule, so it must be denied.
#[cfg(target_os = "macos")]
#[test]
fn test_denies_passwd() {
    let f = Fence::new("passwd");
    let o = f.probe("control", "cat /etc/passwd >/dev/null");
    assert!(
        !o.allowed(),
        "the fence is not enforcing: reading the system password file succeeded, \
         which invalidates every other probe in this file"
    );
    assert!(
        o.denied_by_fence(),
        "expected a fence denial, got exit {:?} with stderr: {}",
        o.code,
        o.stderr
    );
}

/// An allow-listed file under etc must be readable. This is the regression
/// guard for the dead allow segment: the etc paths file exists on every macOS
/// host and is named by the allow list, so a denial here means the segment is
/// unreachable again.
#[cfg(target_os = "macos")]
#[test]
fn test_reads_etc() {
    let f = Fence::new("etc");
    let o = f.probe("etc-leaf", "cat /etc/paths >/dev/null");
    assert!(
        o.allowed(),
        "an allow-listed file under etc must be readable; a denial means the \
         allow segment is unreachable, most likely because the etc link node \
         itself is denied again. stderr: {}",
        o.stderr
    );
}

/// Resolving a path under etc must reach the filesystem even when the target
/// does not exist. The distinction matters: a missing file and a fence denial
/// both fail, but only the denial reports that the operation is not permitted.
/// This is the probe that originally exposed the link-node denial.
#[cfg(target_os = "macos")]
#[test]
fn test_resolves_etc() {
    let f = Fence::new("resolve");
    let o = f.probe("etc-stat", "ls -l /etc/gitconfig");
    assert!(
        !o.denied_by_fence(),
        "path resolution under etc must reach the filesystem for an allow-listed \
         entry, so an absent file reports as absent rather than as denied. \
         stderr: {}",
        o.stderr
    );
}

/// The agent's own config home must not be writable from inside the fence.
///
/// That directory holds the settings the fence itself is rendered from,
/// including the network posture. A run that can write there can widen its own
/// fence for the next run, which converts a single-turn foothold into a durable
/// one and defeats the containment axis without ever tripping it. The write is
/// the whole vector: reads of the settings are harmless.
///
/// Written as a directory probe rather than a probe of the settings file
/// because the rules are path-based, so write access to the directory is
/// equivalent to write access to every file the loader reads from it, and
/// because probing a real settings file cannot be done without risking the
/// user's configuration.
#[cfg(target_os = "macos")]
#[test]
fn test_denies_config_home_write() {
    let f = Fence::new("confighome");
    let dir = PathBuf::from(&f.home).join(".houyicoder");
    if dir.exists() {
        let probe = dir.join(".houyi-fence-write-probe");
        let o = f.probe(
            "create-file",
            "printf '' >> \"$HOME/.houyicoder/.houyi-fence-write-probe\"",
        );
        // Checked and removed from outside the fence: if the write slipped
        // through then the file is real, and it must not outlive the probe.
        let leaked = probe.exists();
        remove_file(&probe).ok();
        assert!(
            !o.allowed() && !leaked,
            "creating a file in the agent's own config home must be denied, or a \
             run can rewrite the settings its next fence is built from. {}",
            o.detail()
        );
        assert!(
            o.denied_by_fence(),
            "expected the fence to refuse the write rather than the command to \
             fail for its own reasons. {}",
            o.detail()
        );
    } else {
        let o = f.probe("create-dir", "mkdir -p \"$HOME/.houyicoder\"");
        let leaked = dir.exists();
        remove_dir(&dir).ok();
        assert!(
            !o.allowed() && !leaked,
            "creating the agent's own config home must be denied, or a run can \
             plant the settings its next fence is built from. {}",
            o.detail()
        );
    }
}

/// An ordinary write inside the workspace must succeed.
///
/// This is the positive counterpart to the password-file probe, and it is load
/// bearing for every denial asserted in this file. A denial only means the rule
/// under test is doing its job if writes that are supposed to work do work. If
/// the workspace allow itself were ineffective, say because the path it names
/// never matches what the kernel resolves, then every write probe here would be
/// denied for a reason that has nothing to do with the rule it claims to
/// exercise, and the whole file would pass while testing nothing.
#[cfg(target_os = "macos")]
#[test]
fn test_allows_workspace_write() {
    let f = Fence::new("wswrite");
    let o = f.probe("ordinary", "printf 'x' >> probe.txt");
    assert!(
        o.allowed(),
        "an ordinary write inside the workspace must be allowed; a denial here \
         means the workspace allow does not match what the kernel resolves, \
         which would make every write denial in this file vacuous. {}",
        o.detail()
    );
}

/// A skill directory inside the workspace is readable but not writable.
///
/// This is the kernel-level proof of the resource-layer posture: the
/// agent may read a skill's reference and script files, but cannot plant or
/// rewrite a SKILL.md. The native skills dir is write-only-denied (reads
/// allowed back), so the broad read-allow + the workspace allow-back reach
/// it for reads, while the write-deny overrides the workspace write-allow
/// (last-match-wins). A string assertion on the rendered rule cannot show
/// the write-deny actually fires against the workspace's own write-allow,
/// nor that the read-allow survives the deny segment — only the kernel
/// verdict does.
#[cfg(target_os = "macos")]
#[test]
fn test_allows_skill_dir_read() {
    let f = Fence::new("skillread");
    let dir = f.workspace.join(".houyicoder/skills/commit");
    create_dir_all(&dir).expect("create skill dir");
    write(dir.join("reference.md"), "skill reference content").expect("seed reference");
    let read = f.probe("read-ref", "cat .houyicoder/skills/commit/reference.md");
    assert!(
        read.allowed(),
        "a skill's reference file must be readable (resource layer): {}",
        read.detail()
    );
}

/// The write counterpart: planting a SKILL.md inside a workspace skill dir
/// must be denied even though the workspace allow-back permits writes
/// there. The write-only skill-dir deny must override the workspace
/// write-allow, or an agent could author a SKILL.md that hijacks a later
/// invocation.
#[cfg(target_os = "macos")]
#[test]
fn test_denies_skill_dir_write() {
    let f = Fence::new("skillwrite");
    let dir = f.workspace.join(".houyicoder/skills/commit");
    create_dir_all(&dir).expect("create skill dir");
    let probe = dir.join("SKILL.md");
    let write = f.probe(
        "plant-skill",
        "printf x >> .houyicoder/skills/commit/SKILL.md",
    );
    let leaked = probe.exists();
    remove_file(&probe).ok();
    assert!(
        !write.allowed() && !leaked,
        "planting a SKILL.md in the skill dir must be denied: {}",
        write.detail()
    );
}

/// The ecosystem and agents skill dirs are read-denied at the kernel fence,
/// unlike the native skills dir whose reads the resource layer opens. A
/// .claude/skills or .agents/skills file is refused on read so a crafted
/// definition an attacker rewrote cannot reach the model through the fence —
/// the source split reserves the resource layer for the native dir. Pins the
/// deny side that test_allows_skill_dir_read's allow asserts against.
#[cfg(target_os = "macos")]
#[test]
fn test_denies_ecosystem_skill_read() {
    let f = Fence::new("skillclaude");
    for family in [".claude/skills", ".agents/skills"] {
        let dir = f.workspace.join(family).join("commit");
        create_dir_all(&dir).expect("create skill dir");
        write(dir.join("reference.md"), "x").expect("seed reference");
        let read = f.probe(
            "read-eco-skill",
            &format!("cat {family}/commit/reference.md"),
        );
        assert!(
            !read.allowed(),
            "a {family} read must be denied (no resource layer for ecosystem/agents source): {}",
            read.detail()
        );
        assert!(
            read.denied_by_fence(),
            "the denial must come from the fence, not an ordinary failure: {}",
            read.detail()
        );
    }
}

/// The persisted permission rules must not be writable from inside the fence.
///
/// Those files are the record of what the user has already consented to, so a
/// run that can append to one grants itself standing approval for whatever it
/// writes there. That is a different and worse failure than widening the fence:
/// it defeats the consent axis rather than the containment axis, and it does so
/// silently, because a rule that is present looks exactly like a rule the user
/// added.
///
/// Both writable scopes are probed. The project scope sits inside the workspace,
/// which is writable by design, and the local scope sits under the temp
/// directory, which the profile allows outright. Neither is covered by the home
/// region deny that protects the user scope, so this is the probe that decides
/// whether that gap is real.
#[cfg(target_os = "macos")]
#[test]
fn test_denies_permission_store_write() {
    let f = Fence::new("permstore");
    let project = f.workspace.join(".houyicoder");
    create_dir_all(&project).expect("create project store dir");
    let project_probe = f.probe("project-scope", "printf '' >> .houyicoder/permissions.json");
    assert!(
        !project_probe.allowed(),
        "the project-scope permission rules must not be writable inside the \
         fence: a run that can write them grants itself standing approval for \
         every later turn. {}",
        project_probe.detail()
    );

    let local = f.tmpdir.join("houyicoder-permissions");
    create_dir_all(&local).expect("create local store dir");
    let local_probe = f.probe(
        "local-scope",
        "printf '' >> \"$TMPDIR/houyicoder-permissions/permissions.json\"",
    );
    assert!(
        !local_probe.allowed(),
        "the local-scope permission rules must not be writable inside the \
         fence, even though the temp directory is allowed for scratch use. {}",
        local_probe.detail()
    );

    // The deny has to stay narrow. Worktrees are created under the same config
    // directory, so a session can legitimately be working inside it, and a guard
    // written against the directory rather than the two files would refuse the
    // agent every write in its own workspace.
    create_dir_all(f.workspace.join(".houyicoder/worktrees/probe")).expect("create worktree dir");
    let worktree = f.probe(
        "worktree-file",
        "printf 'x' >> .houyicoder/worktrees/probe/source.rs",
    );
    assert!(
        worktree.allowed(),
        "ordinary files under the worktree directory must stay writable: a \
         session can have its workspace there, so a guard broad enough to cover \
         the whole config directory would break it. {}",
        worktree.detail()
    );
}

/// Git must work inside the fence with no environment workaround. It reads its
/// system and home config on every invocation and treats a denied read as
/// fatal, so this exercises both allow-backs at once.
#[cfg(target_os = "macos")]
#[test]
fn test_runs_git() {
    let f = Fence::new("git");
    let o = f.probe("git-config", "git config --list >/dev/null");
    assert!(
        o.allowed(),
        "git must read its config inside the fence without any environment \
         workaround. stderr: {}",
        o.stderr
    );
}

/// A repository initialized inside the session temporary directory works: git
/// writes .git/hooks, .git/config, and .gitmodules there. The same paths
/// inside the workspace stay refused, the shared /tmp root stays refused, and
/// the permission store under the temporary directory keeps its own refusal.
#[cfg(target_os = "macos")]
#[test]
fn test_tmpdir_git_init() {
    let f = Fence::new("git-init");
    let init = f.probe(
        "git-init",
        "git init -q \"$TMPDIR/repo\" \
         && test -f \"$TMPDIR/repo/.git/config\" \
         && test -d \"$TMPDIR/repo/.git/hooks\" \
         && printf x > \"$TMPDIR/repo/.gitmodules\" \
         && printf x > \"$TMPDIR/repo/.ripgreprc\"",
    );
    assert!(
        init.allowed(),
        "a repository and dotfiles in the session tmpdir must be writable: {}",
        init.detail()
    );

    // The shared temp root stays out of reach: the allowance names the session
    // temporary directory, not the directory every process shares.
    let shared = f.probe("shared-tmp", "printf x > /tmp/houyi-probe-shared");
    assert!(
        !shared.allowed() && shared.denied_by_fence(),
        "a write to the shared temp root must stay refused: {}",
        shared.detail()
    );

    create_dir_all(f.workspace.join(".git/hooks")).expect("create workspace hooks dir");
    let hooks = f.probe("workspace-hooks", "printf x > .git/hooks/pre-commit");
    assert!(
        !hooks.allowed() && hooks.denied_by_fence(),
        "a workspace hook must stay refused: {}",
        hooks.detail()
    );
    let config = f.probe("workspace-config", "printf x >> .git/config");
    assert!(
        !config.allowed() && config.denied_by_fence(),
        "a workspace git config must stay refused: {}",
        config.detail()
    );
    let modules = f.probe("workspace-gitmodules", "printf x > .gitmodules");
    assert!(
        !modules.allowed() && modules.denied_by_fence(),
        "workspace .gitmodules must stay refused: {}",
        modules.detail()
    );
}

/// The home git config is readable and not writable. The read is an interim
/// allowance so git works; the write stays denied because a writable git
/// config is a credential-helper injection vector. Skipped when the host has
/// no such file, since then neither verdict would mean anything.
#[cfg(target_os = "macos")]
#[test]
fn test_splits_gitconfig() {
    let f = Fence::new("gitconfig");
    let path = format!("{}/.gitconfig", f.home);
    if !Path::new(&path).exists() {
        println!("[gitconfig] SKIPPED: {path} absent on this host");
        return;
    }
    let read = f.probe("read", "cat \"$HOME/.gitconfig\" >/dev/null");
    assert!(
        read.allowed(),
        "the home git config read is allowed back so git works. stderr: {}",
        read.stderr
    );
    let write = f.probe("write", "printf '' >> \"$HOME/.gitconfig\"");
    assert!(
        !write.allowed(),
        "the home git config write must stay denied even though the read is \
         allowed, or a credential helper can be injected into the file every \
         later git invocation reads"
    );
    assert!(
        write.denied_by_fence(),
        "expected a fence denial on the write, got exit {:?} with stderr: {}",
        write.code,
        write.stderr
    );
}
