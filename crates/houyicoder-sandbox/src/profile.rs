//! Composes the macOS Seatbelt profile from process, filesystem, network, and
//! mandatory-deny segments. The profile denies by default, grants only named
//! capabilities, and emits mandatory denies after allow-backs because Seatbelt
//! resolves matching rules in order.

use houyicoder_api::sandbox::NetworkPolicy;
use std::path::Path;

mod deny;
mod network;

use self::deny::{
    deny_home_snapshot_store, deny_permission_stores, deny_settings_files,
    deny_shared_temp_guarded, deny_snapshot_store, deny_snapshot_stores_anywhere, mandatory_deny,
};
use self::network::network_rules;
use std::env::var;

/// Mach services required for process startup and native UI automation.
const MACH_SERVICES: &[&str] = &[
    "com.apple.audio.systemsoundserver",
    "com.apple.distributed_notifications@Uv3",
    "com.apple.FontObjectsServer",
    "com.apple.fonts",
    "com.apple.logd",
    "com.apple.lsd.mapdb",
    "com.apple.PowerManagement.control",
    "com.apple.system.logger",
    "com.apple.system.notification_center",
    "com.apple.system.opendirectoryd.libinfo",
    "com.apple.system.opendirectoryd.membership",
    "com.apple.bsd.dirhelper",
    "com.apple.securityd.xpc",
    "com.apple.coreservices.launchservicesd",
    "com.apple.SecurityServer",
    "com.apple.hiservices-xpcservice",
    // Certificate verification. A child that links the system TLS stack asks
    // trustd to evaluate the chain; without it the handshake fails with an
    // OSStatus error rather than a certificate error.
    "com.apple.trustd",
    "com.apple.trustd.agent",
];

/// /dev devices sh/dyld opens with ioctl. /dev/ptmx is the pseudo-tty master a
/// test harness opens to spawn a terminal pair under the sandbox.
const DEV_IOCTL: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/dtracehelper",
    "/dev/tty",
    "/dev/ptmx",
];

/// Broad system-config read denies. Specific required files are allowed back;
/// sensitive home paths are handled by mandatory_deny.
const DENY_REGIONS: &[&str] = &["/etc", "/private/etc"];

/// Public system files required by shell, git, and build startup. Rendering
/// covers both /etc and its resolved /private/etc form.
const SYSTEM_ETC_ALLOW: &[&str] = &[
    "/etc/profile",
    "/etc/gitconfig",
    "/etc/paths",
    // cargo links the system OpenSSL/LibreSSL, which reads this public
    // trust-store config at process init; denying it breaks cargo.
    "/etc/ssl/openssl.cnf",
    // The public CA bundle curl and LibreSSL load. Both are world-readable and
    // contain no secret; without the bundle a TLS client reports a missing CA.
    "/etc/ssl/cert.pem",
    // Local time, so a child reports the host zone instead of UTC.
    "/etc/localtime",
];

/// Render a complete profile. Mandatory denies follow allow-backs so they
/// remain authoritative.
pub fn render_profile(workspace: &Path, tmpdir: &str, tag: &str) -> String {
    let home = var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
    render(&ProfileSpec::new(workspace, tmpdir, &home, tag))
}

/// Runtime directory allow-backs split by write capability.
#[derive(Debug, Clone, Copy)]
pub struct AdditionalDirs<'a> {
    /// Directories available to reads but not writes.
    pub read_only: &'a [&'a str],
    /// Directories available to both reads and writes.
    pub read_write: &'a [&'a str],
}

impl AdditionalDirs<'_> {
    fn empty() -> Self {
        Self {
            read_only: &[],
            read_write: &[],
        }
    }
}

/// Named inputs for rendering a complete profile.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ProfileSpec<'a> {
    /// The workspace root the fence is bound to.
    pub workspace: &'a Path,
    /// The temp dir the sandboxed child may write to.
    pub tmpdir: &'a str,
    /// The home dir the mandatory credential denies are placed under. Explicit
    /// rather than read from the environment so tests are deterministic.
    pub home: &'a str,
    /// The violation message tag, used to attribute a kernel denial back to
    /// this session.
    pub tag: &'a str,
    /// Runtime-added directories separated by write capability.
    pub additional: AdditionalDirs<'a>,
    /// Additional validated Mach service names.
    pub extra_mach_services: &'a [&'a str],
    /// Whether app-launch and its required service lookups are allowed.
    pub allow_app_launch: bool,
    /// How wide the network fence is opened. Defaults to fully contained.
    pub network: NetworkPolicy,
}

impl<'a> ProfileSpec<'a> {
    /// A spec with the required inputs and the narrowest optional ones: no
    /// additional directories, and a fully contained network.
    #[must_use]
    pub fn new(workspace: &'a Path, tmpdir: &'a str, home: &'a str, tag: &'a str) -> Self {
        Self {
            workspace,
            tmpdir,
            home,
            tag,
            additional: AdditionalDirs::empty(),
            extra_mach_services: &[],
            allow_app_launch: false,
            network: NetworkPolicy::contained(),
        }
    }

    /// Set the runtime-added workspace directories.
    #[must_use]
    pub fn with_additional(mut self, additional: &'a [&'a str]) -> Self {
        self.additional.read_write = additional;
        self
    }

    /// Set runtime-added directories that remain read-only.
    #[must_use]
    pub fn with_read_only(mut self, additional: &'a [&'a str]) -> Self {
        self.additional.read_only = additional;
        self
    }

    /// Set extra mach services for skills/tools that need XPC services
    /// beyond the base process-start set.
    #[must_use]
    pub fn with_mach_services(mut self, services: &'a [&'a str]) -> Self {
        self.extra_mach_services = services;
        self
    }

    /// Grant the LaunchServices app-launch entitlement (lsopen + the
    /// coreservicesd and quarantine-resolver mach lookups) so a sandboxed
    /// process can launch an app via open -a.
    #[must_use]
    pub fn with_app_launch(mut self, allow: bool) -> Self {
        self.allow_app_launch = allow;
        self
    }

    /// Set the network policy.
    #[must_use]
    pub fn with_network(mut self, network: NetworkPolicy) -> Self {
        self.network = network;
        self
    }
}

/// Render the profile described by the spec.
pub fn render(spec: &ProfileSpec<'_>) -> String {
    let ProfileSpec {
        workspace,
        tmpdir,
        home,
        tag,
        additional,
        extra_mach_services,
        allow_app_launch,
        network,
    } = spec;
    let (tmpdir, home, tag, allow_app_launch) = (*tmpdir, *home, *tag, *allow_app_launch);
    let mut s = String::new();
    s.push_str("(version 1)\n");
    s.push_str(&deny_default(tag));
    s.push_str(&allow_set(tag));
    if allow_app_launch {
        s.push_str(&allow_app_launch_segment());
    }
    // Extra mach services declared by a skill or tool. Validated
    // alphanumeric+dot+dash so a frontmatter value cannot inject
    // seatbelt directives via the interpolated name.
    for name in extra_mach_services.iter() {
        if name.is_empty() {
            continue;
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
        {
            continue;
        }
        s.push_str(&format!("(allow mach-lookup (global-name \"{name}\"))\n"));
    }
    s.push_str(&filesystem_rules_with_access(
        workspace,
        tmpdir,
        tag,
        additional.read_only,
        additional.read_write,
    ));
    // The shared temp root is writable: tools that hard-code /tmp cannot be
    // redirected, and a session that cannot write there fails on programs it
    // does not control. The subtrees other processes own stay closed.
    s.push_str("(allow file-read* file-write* (subpath \"/private/tmp\") (subpath \"/tmp\"))\n");
    s.push_str(&deny_shared_temp_guarded(tag));
    // Re-emits any deny region nested inside a path that was just opened for
    // writing, so a workspace under /Users keeps the sibling denies.
    let allow_paths: Vec<&str> = {
        let mut v: Vec<&str> = vec![workspace.to_str().unwrap_or(""), tmpdir];
        v.extend(additional.read_only.iter().copied());
        v.extend(additional.read_write.iter().copied());
        v
    };
    s.push_str(&deny_read_always(DENY_REGIONS, &allow_paths, tag));
    // Lands after the allow-backs so opening the workspace for writing cannot
    // wash out a deny of a credential path inside it.
    s.push_str(&mandatory_deny(home, tag));
    // Settings and the anywhere-snapshot deny land before the scratch
    // directory is opened for writing, so a fixture project gets its own
    // settings file and may write snapshots under the scratch or shared temp
    // root. Permission stores and the workspace/home snapshot denies land
    // after it, so those stay write-denied even when placed under a writable
    // root.
    s.push_str(&deny_settings_files(home, tag));
    s.push_str(&deny_snapshot_stores_anywhere(tag));
    s.push_str(&allow_tmpdir_writes(tmpdir, workspace));
    s.push_str(&allow_temp_entries());
    s.push_str(&deny_permission_stores(home, tag));
    s.push_str(&deny_snapshot_store(workspace, tag));
    s.push_str(&deny_home_snapshot_store(home, tag));
    // After the deny above, so last-match-wins lets git read the system copies.
    // The home copies stay denied.
    s.push_str(&allow_system_etc(tag));
    // The home git config read, allowed back after both the broad home-region
    // deny and the write-only mandatory entry. Interim posture, see the
    // builder's documentation: writes stay denied, reads are allowed so git
    // works, and transcript-side redaction is the real fix.
    s.push_str(&allow_home_gitconfig(home, tag));
    // After the credential deny, so last-match-wins lets the ssh client read
    // the tree its host-key and identity configuration lives in.
    s.push_str(&allow_ssh_client_reads(home, tag));
    s.push_str(&network_rules(network, tag));
    s
}

/// Re-allow reads of SYSTEM_ETC_ALLOW after the deny of the /etc tree, so git
/// can read its system config. Writes stay denied.
pub fn allow_system_etc(tag: &str) -> String {
    let _ = tag;
    // /etc resolves to /private/etc and the kernel matches the resolved path, so
    // both spellings are emitted from the one list.
    let mut literals = Vec::new();
    for p in SYSTEM_ETC_ALLOW {
        literals.push(format!("(literal \"{p}\")"));
        let resolved = p.replacen("/etc", "/private/etc", 1);
        if resolved != *p {
            literals.push(format!("(literal \"{resolved}\")"));
        }
    }
    // The leaf rules above never fire without this: the deny covers the /etc
    // symlink node, so the kernel refuses while resolving the path. Metadata
    // only, so nothing under /etc becomes readable.
    format!(
        "(allow file-read* {})\n(allow file-read-metadata (literal \"/etc\"))\n",
        literals.join(" ")
    )
}

/// Re-allow reads of the home git config. Git treats a denied config as fatal,
/// so without this every git command in the session fails. Writes stay denied.
///
/// Interim: the file can carry a credential section, which reaches the
/// transcript. Redacting values at the transcript boundary replaces this. A
/// minimal config passed through the environment was rejected because it drops
/// aliases and url rewrites and leaves the secret readable by other means.
pub fn allow_home_gitconfig(home: &str, tag: &str) -> String {
    let _ = tag;
    format!("(allow file-read* (literal \"{home}/.gitconfig\"))\n")
}

/// Re-allow reads of the ssh tree for the ssh client itself.
///
/// An ssh remote cannot authenticate without reading its host-key store, its
/// configuration, and the identity file that configuration names. Denying the
/// whole tree makes every ssh remote unusable, which is what a push and a fetch
/// both are when the remote is an ssh URL.
///
/// The read is bound to the client binary, not to the path: a shell, a script,
/// or any other program that tries to read the same tree is still refused by
/// the credential deny, so the key never reaches a command the model wrote.
/// Writes stay denied for every process, the client included.
pub fn allow_ssh_client_reads(home: &str, tag: &str) -> String {
    let _ = tag;
    let mut out = String::new();
    for program in SSH_CLIENTS {
        out.push_str(&format!(
            "(allow file-read* (require-all (subpath \"{home}/.ssh\") (process-path \"{program}\")))\n"
        ));
    }
    out
}

/// The binaries that authenticate an ssh remote: the client and the two
/// file-transfer programs built on it, at the paths a package manager installs
/// them to. The list is literal, not a pattern, so a program the agent could
/// write is never one of them; a wider list would hand the key to a program
/// that is not an ssh client.
const SSH_CLIENTS: &[&str] = &[
    "/usr/bin/ssh",
    "/usr/bin/scp",
    "/usr/bin/sftp",
    "/opt/homebrew/bin/ssh",
    "/opt/homebrew/bin/scp",
    "/opt/homebrew/bin/sftp",
    "/usr/local/bin/ssh",
    "/usr/local/bin/scp",
    "/usr/local/bin/sftp",
];

/// (deny default (with message tag)) — the catch-all. Every operation not
/// explicitly allowed is denied; the tag attributes the violation in
/// log stream output for a future monitor.
pub fn deny_default(tag: &str) -> String {
    format!("(deny default (with message \"{tag}\"))\n")
}

/// The allow-set: everything sh/dyld needs to start a process under
/// sandbox-exec — process exec/fork/info/signal, mach-priv-task-port,
/// mach-lookup of 16 system services, ipc-posix-shm/sem, iokit,
/// system-socket AF_SYSTEM proto 2, sysctl-read, distributed-notification,
/// file-ioctl on /dev/null|zero|random|urandom|dtracehelper|tty|ptmx, and
/// read-write on /dev/null character device. The pseudo-tty allow and the
/// read-write on /dev/ptmx and /dev/ttys* let a test harness open a terminal
/// pair under the sandbox. Without this set sh aborts under deny default.
///
/// Signal is not restricted to this sandbox: sending one is a control action
/// the permission gate decides, and a kernel rule that refused an approved
/// signal would overrule the user's own decision, leaving a command unable to
/// stop a process it started. Task ports stay restricted, because reaching
/// into another process's memory is not something an approval can make
/// recoverable.
///
/// The literal /bin/ps runs with no-sandbox because macOS refuses to execve a
/// setuid binary inside a sandbox seatbelt. The kernel will not both raise
/// privilege and confine it. ps is a read-only leaf on the SIP-protected
/// system volume with no spawn or write surface, so the exception is narrow
/// and does not extend to the shell or to any path-prefix match. The broad
/// process-exec allow above does not help, because setuid is a kernel gate,
/// not a profile permission.
pub fn allow_set(tag: &str) -> String {
    let _ = tag;
    let mach = MACH_SERVICES
        .iter()
        .map(|s| format!("(global-name \"{s}\")"))
        .collect::<Vec<_>>()
        .join(" ");
    let dev_ioctl = DEV_IOCTL
        .iter()
        .map(|d| format!("(literal \"{d}\")"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "(allow process-exec)\n\
         (allow process-exec (literal \"/bin/ps\") (with no-sandbox))\n\
         (allow process-fork)\n\
         (allow process-info*)\n\
         (allow signal)\n\
         (allow mach-priv-task-port (target same-sandbox))\n\
         (allow user-preference-read)\n\
         (allow mach-lookup {mach})\n\
         (allow ipc-posix-shm)\n\
         (allow ipc-posix-sem)\n\
         (allow iokit-get-properties)\n\
         (allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))\n\
         (allow sysctl-read)\n\
         (allow distributed-notification-post)\n\
         (allow file-ioctl {dev_ioctl})\n\
         (allow file-read-data file-write-data (require-all (literal \"/dev/null\") (vnode-type CHARACTER-DEVICE)))\n\
         (allow pseudo-tty)\n\
         (allow file-read-data file-write-data file-ioctl (literal \"/dev/ptmx\"))\n\
         (allow file-read-data file-write-data file-ioctl (regex #\"^/dev/ttys[0-9]+$\"))\n"
    )
}

/// The app-launch entitlement: lsopen plus the LaunchServices mach lookups
/// a process needs to resolve and launch an app via open -a. Without these
/// open fails with procNotFound (-600) on macOS 14/15. Emitted only when
/// the spec opts in (a skill declares the need), so a default sandboxed
/// command cannot launch arbitrary apps.
pub fn allow_app_launch_segment() -> String {
    "(allow lsopen)\n\
     (allow mach-lookup (global-name \"com.apple.CoreServices.coreservicesd\"))\n\
     (allow mach-lookup (global-name \"com.apple.coreservices.quarantine-resolver\"))\n"
        .to_string()
}

/// Render filesystem rules with read allow-backs and allow-only writes.
pub fn filesystem_rules(workspace: &Path, tmpdir: &str, tag: &str, additional: &[&str]) -> String {
    filesystem_rules_with_access(workspace, tmpdir, tag, &[], additional)
}

fn filesystem_rules_with_access(
    workspace: &Path,
    tmpdir: &str,
    tag: &str,
    read_only: &[&str],
    read_write: &[&str],
) -> String {
    let ws = workspace.to_string_lossy();
    let deny_regions = DENY_REGIONS
        .iter()
        .map(|region| format!("(subpath \"{region}\")"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut read_subs = format!("(subpath \"{ws}\") (subpath \"{tmpdir}\")");
    for directory in read_only.iter().chain(read_write) {
        if !directory.is_empty() {
            read_subs.push_str(&format!(" (subpath \"{directory}\")"));
        }
    }
    let mut write_subs = format!("(subpath \"{ws}\") (subpath \"{tmpdir}\")");
    for directory in read_write {
        if !directory.is_empty() {
            write_subs.push_str(&format!(" (subpath \"{directory}\")"));
        }
    }
    format!(
        "(allow file-read*)\n\
         (deny file-read* {deny_regions} (with message \"{tag}\"))\n\
         (allow file-read* {read_subs})\n\
         (allow file-write* {write_subs} (literal \"/dev/null\") (literal \"/dev/stdout\") (literal \"/dev/stderr\"))\n"
    )
}

/// Re-allow writes inside the session's private temporary directory. That
/// directory is removed on drop and never opened by the user, so a command may
/// initialize repositories or lay out dotfiles there freely; the denies it
/// lands after protect trees that outlive the session.
///
/// Empty when the directory is unset or contains the workspace, so a workspace
/// inside a shared temp root keeps its own denies.
fn allow_tmpdir_writes(tmpdir: &str, workspace: &Path) -> String {
    let trimmed = tmpdir.trim_end_matches('/');
    if trimmed.is_empty() || workspace.starts_with(Path::new(trimmed)) {
        return String::new();
    }
    format!("(allow file-write* (subpath \"{trimmed}\"))\n")
}

/// Allow creating and using one temporary entry directly under the user's
/// Darwin temp root.
///
/// A bare mktemp does not read TMPDIR on this platform: it asks confstr for
/// the per-user temp root and creates the entry there. A session that can only
/// write its own scratch directory therefore fails every script that calls
/// mktemp without a template, which is most of them, including this
/// repository's own pre-push hook.
///
/// The rule is tight on purpose. It admits a single entry whose name starts
/// with the prefix mktemp itself uses, under the temp root, and nothing else:
/// the siblings in that directory are other programs' fixtures and stay
/// closed. Both spellings are emitted because the kernel matches the resolved
/// path.
pub fn allow_temp_entries() -> String {
    "(allow file-read* file-write* (regex #\"^/private/var/folders/[^/]+/[^/]+/T/tmp\\.[A-Za-z0-9]+(/|$)\"))\n\
     (allow file-read* file-write* (regex #\"^/var/folders/[^/]+/[^/]+/T/tmp\\.[A-Za-z0-9]+(/|$)\"))\n"
        .to_string()
}

/// Re-emit nested read denies after allow-backs so they remain authoritative.
pub fn deny_read_always(deny_paths: &[&str], allow_paths: &[&str], tag: &str) -> String {
    let mut out = String::new();
    for deny in deny_paths {
        let nested = allow_paths
            .iter()
            .any(|allow| !allow.is_empty() && deny.starts_with(&format!("{allow}/")));
        if nested {
            out.push_str(&format!(
                "(deny file-read* (subpath \"{deny}\") (with message \"{tag}\"))\n"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests;
