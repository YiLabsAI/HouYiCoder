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
//! subsystem, which the unit suite is required never to do. They therefore run
//! in the heavier pre-push suite rather than the commit gate, which is unit
//! only by policy; the suite picks this binary up as an ordinary test target,
//! so no special wiring is involved. Everything is compiled out where there is
//! no seatbelt.
//!
//! Human-readable report, useful when debugging a profile change, run from
//! this crate's directory:
//!   cargo test --test seatbelt_kernel -- --nocapture
//! Set HOUYI_PROFILE_DUMP=1 to also print the full rendered profile.

// These probes must invoke sandbox-exec directly: the thing under test IS the
// kernel's verdict on a rendered profile, so routing the spawn through the
// launcher would put the subject behind one of its own consumers. Same
// reasoning as the seatbelt test scaffolding. Production code still routes
// every spawn through the launcher.
#![allow(clippy::disallowed_methods)]

/// A live fence: a throwaway workspace plus the profile rendered for it. The
/// workspace is removed on drop so a failing assertion cannot leave a
/// directory behind.
#[cfg(target_os = "macos")]
struct Fence {
    workspace: std::path::PathBuf,
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
    elapsed: std::time::Duration,
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
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
        // Canonicalized because the session does the same before rendering, and
        // the kernel matches rules against the resolved path. On this platform
        // the temp directory is reached through a symlink, so the unresolved
        // form names a path the kernel never sees: every allow under it would
        // silently fail to match, and every write probe would then be denied for
        // that reason rather than by the rule it means to exercise.
        let tmpdir = dunce::canonicalize(std::env::temp_dir()).expect("canonicalize temp dir");
        let tag = format!(
            "houyi-fence-{label}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let workspace = tmpdir.join(&tag);
        std::fs::create_dir_all(&workspace).expect("create probe workspace");
        let profile = houyicoder_sandbox::render(&houyicoder_sandbox::ProfileSpec::new(
            &workspace,
            &tmpdir.to_string_lossy(),
            &home,
            &tag,
        ));
        if std::env::var("HOUYI_PROFILE_DUMP").is_ok() {
            println!("{profile}");
        }
        Self {
            workspace,
            profile,
            home,
        }
    }

    /// Render the real profile with an explicit network posture. Separate from
    /// new so the filesystem probes keep the contained default they assert
    /// against, and so a network probe states its posture at the call site.
    fn with_network(label: &str, network: houyicoder_api::sandbox::NetworkPolicy) -> Self {
        let mut f = Self::new(label);
        let tmpdir = dunce::canonicalize(std::env::temp_dir()).expect("canonicalize temp dir");
        f.profile = houyicoder_sandbox::render(
            &houyicoder_sandbox::ProfileSpec::new(
                &f.workspace,
                &tmpdir.to_string_lossy(),
                &f.home,
                label,
            )
            .with_network(network),
        );
        f
    }

    /// Run one shell command inside the fence and report the verdict.
    fn probe(&self, label: &str, cmd: &str) -> Outcome {
        let started = std::time::Instant::now();
        let out = std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg(&self.profile)
            .arg("--")
            .arg("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.workspace)
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
        std::fs::remove_dir_all(&self.workspace).ok();
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
    let dir = std::path::PathBuf::from(&f.home).join(".houyicoder");
    if dir.exists() {
        let probe = dir.join(".houyi-fence-write-probe");
        let o = f.probe(
            "create-file",
            "printf '' >> \"$HOME/.houyicoder/.houyi-fence-write-probe\"",
        );
        // Checked and removed from outside the fence: if the write slipped
        // through then the file is real, and it must not outlive the probe.
        let leaked = probe.exists();
        std::fs::remove_file(&probe).ok();
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
        std::fs::remove_dir(&dir).ok();
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
    std::fs::create_dir_all(&project).expect("create project store dir");
    let project_probe = f.probe("project-scope", "printf '' >> .houyicoder/permissions.json");
    assert!(
        !project_probe.allowed(),
        "the project-scope permission rules must not be writable inside the \
         fence: a run that can write them grants itself standing approval for \
         every later turn. {}",
        project_probe.detail()
    );

    let local = std::env::temp_dir().join("houyicoder-permissions");
    std::fs::create_dir_all(&local).expect("create local store dir");
    let local_probe = f.probe(
        "local-scope",
        "printf '' >> \"$TMPDIR/houyicoder-permissions/permissions.json\"",
    );
    let leaked = local.join("permissions.json");
    let existed = leaked.exists();
    if existed {
        std::fs::remove_file(&leaked).ok();
    }
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
    std::fs::create_dir_all(f.workspace.join(".houyicoder/worktrees/probe"))
        .expect("create worktree dir");
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

/// The home git config is readable and not writable. The read is an interim
/// allowance so git works; the write stays denied because a writable git
/// config is a credential-helper injection vector. Skipped when the host has
/// no such file, since then neither verdict would mean anything.
#[cfg(target_os = "macos")]
#[test]
fn test_splits_gitconfig() {
    let f = Fence::new("gitconfig");
    let path = format!("{}/.gitconfig", f.home);
    if !std::path::Path::new(&path).exists() {
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

// ---- network posture probes ------------------------------------------------
//
// Every probe below uses a literal address and never a hostname, so none of them
// depends on DNS or on the host having working internet.
//
// The verbose flag on the network tool is load-bearing, not decoration. Without
// it the tool is silent on every failure, which leaves a kernel refusal and a
// permitted-but-unanswered connect indistinguishable and forces the verdict onto
// a latency threshold. That would make these the only probes here whose result
// depends on something other than the kernel, because a network that answers the
// unreachable address with a reset fails just as fast as the fence does. With the
// flag the tool prints the kernel's own wording, which is the same
// not-permitted text the filesystem probes match on, so the network probes use
// the same judgement as everything else in this file and a network-level refusal
// reads as what it is rather than as a fence denial. The elapsed time is still
// reported, as evidence in the output, but nothing asserts on it.

/// The egress target: an address that cannot be routed anywhere.
///
/// A hardcoded literal on purpose, and not a value worth making configurable.
/// This is the documentation-only range reserved by RFC 5737, and the standard
/// reserving it is exactly what guarantees the property the probe needs. Any
/// substitute loses that guarantee and is worse for a specific reason: an address
/// that turns out to be reachable would have the open-posture probe open a real
/// connection to a stranger's host from the test suite, and an address from a
/// private range may well be routable on whatever network the machine is on and
/// reach a real device there. Loopback is not an alternative either, since that
/// is the separate axis the loopback probes cover.
#[cfg(target_os = "macos")]
const UNROUTABLE: &str = "192.0.2.1";

/// Bind a loopback listener in the test process and hand back its port, so a
/// probe can attempt a real loopback connection with a real peer on the other
/// end. Kept alive by the returned listener; dropping it closes the port.
#[cfg(target_os = "macos")]
fn loopback_listener() -> (std::net::TcpListener, u16) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = l.local_addr().expect("listener addr").port();
    (l, port)
}

/// The contained posture must actually stop egress at the kernel. This is the
/// baseline claim the whole containment story rests on, so it is asserted
/// against the kernel rather than against the profile text.
#[cfg(target_os = "macos")]
#[test]
fn test_contained_blocks_egress() {
    let f = Fence::with_network(
        "net-off",
        houyicoder_api::sandbox::NetworkPolicy::contained(),
    );
    let o = f.probe("egress", &format!("nc -v -G 1 -w 1 -z {UNROUTABLE} 80"));
    assert!(!o.allowed(), "the contained posture must refuse egress");
    assert!(
        o.denied_by_fence(),
        "the refusal must come from the fence rather than from the network: the \
         kernel names its own verdict on stderr, and a permitted connect to the \
         unreachable target reports a timeout instead. {}",
        o.detail()
    );
}

/// The open posture must actually reach the kernel too. Without this the
/// previous probe could pass for the wrong reason — a profile that denied
/// everything regardless of policy would satisfy it.
#[cfg(target_os = "macos")]
#[test]
fn test_open_permits_egress() {
    let mut p = houyicoder_api::sandbox::NetworkPolicy::contained();
    p.egress = houyicoder_api::sandbox::Egress::Unrestricted;
    let f = Fence::with_network("net-open", p);
    let o = f.probe("egress", &format!("nc -v -G 1 -w 1 -z {UNROUTABLE} 80"));
    assert!(
        !o.denied_by_fence(),
        "the open posture must let the connect through, which the unroutable \
         target then leaves to time out; a not-permitted verdict means the fence \
         blocked it. {}",
        o.detail()
    );
}

/// Loopback must be unreachable by default. A dev server left reachable under
/// the default posture would be an opening nobody asked for.
#[cfg(target_os = "macos")]
#[test]
fn test_contained_blocks_loopback() {
    let (_l, port) = loopback_listener();
    let f = Fence::with_network(
        "net-lo-off",
        houyicoder_api::sandbox::NetworkPolicy::contained(),
    );
    let o = f.probe("loopback", &format!("nc -v -G 1 -w 1 -z 127.0.0.1 {port}"));
    assert!(!o.allowed(), "loopback is not reachable by default");
    assert!(
        o.denied_by_fence(),
        "expected a fence denial, not a timeout. {}",
        o.detail()
    );
}

/// The loopback opt-in must reach loopback while still refusing egress.
///
/// This is the probe that pins the trap. The outbound rule that makes loopback
/// work has to filter on the remote address; spelled with a local-address filter
/// instead it admits every host, because the source address of an unbound socket
/// is the any-address at connect time and the loopback token matches it. Both
/// halves are therefore asserted together: reaching loopback proves the rule
/// fires, and the egress refusal in the same fence proves it did not fire wider
/// than intended. Splitting them into separate tests would let the dangerous
/// half regress while the reassuring half stayed green.
#[cfg(target_os = "macos")]
#[test]
fn test_loopback_grants_no_egress() {
    let (_l, port) = loopback_listener();
    let mut p = houyicoder_api::sandbox::NetworkPolicy::contained();
    p.allow_local_binding = true;
    let f = Fence::with_network("net-lo-on", p);
    let lo = f.probe("loopback", &format!("nc -v -G 1 -w 1 -z 127.0.0.1 {port}"));
    assert!(
        lo.allowed(),
        "the loopback opt-in must actually reach a loopback peer. stderr: {}",
        lo.stderr
    );
    let out = f.probe("egress", &format!("nc -v -G 1 -w 1 -z {UNROUTABLE} 80"));
    assert!(
        out.denied_by_fence(),
        "the loopback opt-in must not widen egress; a timeout here instead of a \
         fence denial means the outbound rule matched every host, which is the \
         source-address trap. {}",
        out.detail()
    );
}

/// Unix sockets must be unreachable by default, and reachable when listed.
///
/// Both halves live in one probe because the interesting property is the
/// difference: the default denial is only meaningful if the same operation
/// succeeds once the path is allowed, which also proves the three separate
/// operations the allow-back emits are in fact all the operations needed.
#[cfg(target_os = "macos")]
#[test]
fn test_socket_allowback_works() {
    // Canonicalized because the fence matches the resolved realpath: the temp
    // dir is reached through a symlink on macOS, so an allow-back naming the
    // unresolved path would render a rule the kernel never matches. This is the
    // same failure that made the system-etc segment dead for a long time.
    let dir = std::env::temp_dir().join(format!("houyi-sock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create socket dir");
    let dir = std::fs::canonicalize(&dir).expect("canonicalize socket dir");
    let path = dir.join("probe.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind unix socket");
    let cmd = format!("nc -v -w 1 -U {} </dev/null", path.display());

    let denied = Fence::with_network(
        "sock-off",
        houyicoder_api::sandbox::NetworkPolicy::contained(),
    );
    let off = denied.probe("socket", &cmd);
    assert!(
        !off.allowed(),
        "unix sockets are denied by default; the network class deny covers them"
    );

    let mut p = houyicoder_api::sandbox::NetworkPolicy::contained();
    p.unix_sockets =
        houyicoder_api::sandbox::UnixSockets::Paths(vec![dir.to_string_lossy().into_owned()]);
    let allowed = Fence::with_network("sock-on", p);
    let on = allowed.probe("socket", &cmd);
    assert!(
        on.allowed(),
        "a listed unix socket path must be reachable; a denial means one of the \
         three operations the allow-back emits is missing. stderr: {}",
        on.stderr
    );

    drop(listener);
    std::fs::remove_dir_all(&dir).ok();
}
