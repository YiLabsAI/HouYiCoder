//! Composes the macOS Seatbelt profile from process, filesystem, network, and
//! mandatory-deny segments. The profile denies by default, grants only named
//! capabilities, and emits mandatory denies after allow-backs because Seatbelt
//! resolves matching rules in order.

use houyicoder_api::sandbox::NetworkPolicy;
use std::path::Path;

mod deny;
mod network;

use self::deny::{deny_snapshot_store, mandatory_deny};
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
];

/// /dev devices sh/dyld opens with ioctl.
const DEV_IOCTL: &[&str] = &[
    "/dev/null",
    "/dev/zero",
    "/dev/random",
    "/dev/urandom",
    "/dev/dtracehelper",
    "/dev/tty",
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
    /// The home dir used to place the mandatory exfiltration denies. Explicit
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
    // Deny writes to the snapshot store inside the workspace so a destructive
    // command (rm -rf, git clean -fdx) cannot destroy its own undo data.
    // Last-match-wins: this lands after the workspace allow-write-back so the
    // deny takes precedence. The snapshot store is written by the host process
    // (BashTool::execute), not by the sandboxed child, so the deny does not
    // block snapshot creation.
    s.push_str(&deny_snapshot_store(workspace, tag));
    // deny_read_always re-emits any DENY_REGIONS literal that is nested
    // inside an allow-back subpath (workspace/tmpdir). When the workspace
    // lives under a deny region (e.g. /Users/me/...), this keeps the
    // sibling deny paths fired.
    let allow_paths: Vec<&str> = {
        let mut v: Vec<&str> = vec![workspace.to_str().unwrap_or(""), tmpdir];
        v.extend(additional.read_only.iter().copied());
        v.extend(additional.read_write.iter().copied());
        v
    };
    s.push_str(&deny_read_always(DENY_REGIONS, &allow_paths, tag));
    // mandatory_deny is the concrete application of denyReadAlways to the
    // exfiltration vectors: it lands after the allow-back so an allow-back
    // of the workspace cannot wash out a deny of .ssh/.gitconfig inside it.
    s.push_str(&mandatory_deny(home, tag));
    // A repository created inside the session scratch dir needs its own
    // .git/hooks and .git/config, which the mandatory deny above forbids
    // everywhere. The allow names only the scratch dir and only those two
    // entries; the workspace denials are re-emitted after it, so a caller that
    // renders a profile whose workspace sits inside the scratch dir cannot
    // spend the allowance on the repository the user later opens.
    s.push_str(&allow_scratch_git(tmpdir));
    s.push_str(&deny_workspace_git(workspace, tag));
    // System /etc files git reads at startup. Lands AFTER mandatory_deny
    // (whose .profile regex also matches these system paths) so
    // last-match-wins re-allows the reads. The home profile file stays denied
    // — only the system-wide copies are re-allowed here. The shell runs
    // non-login so it does not source /etc/profile; the entry stays for any
    // tool that reads it directly.
    s.push_str(&allow_system_etc(tag));
    // The home git config read, allowed back after both the broad home-region
    // deny and the write-only mandatory entry. Interim posture, see the
    // builder's documentation: writes stay denied, reads are allowed so git
    // works, and transcript-side redaction is the real fix.
    s.push_str(&allow_home_gitconfig(home, tag));
    s.push_str(&network_rules(network, tag));
    s
}

/// Re-allow reads of SYSTEM_ETC_ALLOW after the broad /etc deny and the
/// mandatory rc-file regex denies. Last-match-wins lets these
/// specific system files through so git can read /etc/gitconfig. The shell is
/// non-login so it does not source /etc/profile; that entry is retained for
/// direct reads. Writes stay denied (read-only allow).
pub fn allow_system_etc(tag: &str) -> String {
    let _ = tag;
    // Emit both the /etc literal and the /private/etc resolved form: on
    // macOS /etc symlinks to /private/etc and seatbelt matches the resolved
    // realpath, so an allow on /etc/X alone does not reach the read the kernel
    // performs. Covering both here keeps the SYSTEM_ETC_ALLOW list as the
    // single source (one entry per file, no manual /private/etc mirror).
    let mut literals = Vec::new();
    for p in SYSTEM_ETC_ALLOW {
        literals.push(format!("(literal \"{p}\")"));
        let resolved = p.replacen("/etc", "/private/etc", 1);
        if resolved != *p {
            literals.push(format!("(literal \"{resolved}\")"));
        }
    }
    // Allow the metadata read of the /etc node itself. Without this the leaf
    // allows above never fire: /etc is a symlink to /private/etc, and the broad
    // deny of the /etc subpath also covers that symlink node, so the kernel
    // refuses while resolving the path and never evaluates the leaf rule. A
    // real-kernel probe showed git reporting operation-not-permitted on
    // /etc/gitconfig, and a plain read of the allow-listed /etc/paths failing
    // the same way, even though both leaves were allow-listed. Metadata only:
    // this grants resolution of the link, not read of anything under it, so
    // every other path below /etc (the password file included) stays denied.
    format!(
        "(allow file-read* {})\n(allow file-read-metadata (literal \"/etc\"))\n",
        literals.join(" ")
    )
}

/// Re-allow reads of the home git config after the broad home-region deny and
/// the write-only mandatory deny. Git reads this file on every invocation, so
/// a denial does not degrade gracefully: git reports that it cannot access the
/// path and exits non-zero, which breaks every git command in the sandbox
/// rather than merely dropping a preference. Writes stay denied by the
/// write-only mandatory entry, so a credential helper cannot be injected.
///
/// Interim, as documented on the write-only list: the read is allowed so git
/// works and the user's aliases and conditional includes stay faithful, at the
/// cost of a credential section being readable into the transcript. Value-level
/// redaction on the transcript boundary is the real fix and supersedes this.
/// The alternative of passing a minimal config through the environment was
/// rejected: it loses aliases, conditional includes and url rewrites, and the
/// secret would still be reachable by another read path.
pub fn allow_home_gitconfig(home: &str, tag: &str) -> String {
    let _ = tag;
    format!("(allow file-read* (literal \"{home}/.gitconfig\"))\n")
}

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
/// file-ioctl on /dev/null|zero|random|urandom|dtracehelper|tty, and
/// read-write on /dev/null character device. Without this set sh aborts
/// under deny default.
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
         (allow process-fork)\n\
         (allow process-info* (target same-sandbox))\n\
         (allow signal (target same-sandbox))\n\
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
         (allow file-read-data file-write-data (require-all (literal \"/dev/null\") (vnode-type CHARACTER-DEVICE)))\n"
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

/// Re-allow the git internals of a repository created inside the session
/// scratch directory, and only there.
///
/// A test or a timing run in scratch has to be able to initialize its own
/// repository, and that writes .git/hooks and .git/config. Both paths are
/// write-denied so an agent cannot plant a hook or redirect a credential
/// helper in a repository the user later opens outside the fence. Inside
/// scratch nothing outlives the session, so the same denial only stops the
/// run. The rule ends at those two entries rather than covering the directory,
/// so the authority files under scratch keep their own denial.
fn allow_scratch_git(tmpdir: &str) -> String {
    let tmpdir = tmpdir.trim_end_matches('/');
    if tmpdir.is_empty() {
        return String::new();
    }
    let escaped = escape_regex(tmpdir);
    // The repository may sit at the top of the scratch dir or in a
    // subdirectory, so the middle segment is optional.
    format!("(allow file-write* (regex #\"^{escaped}/(.*/)?\\.git/(hooks(/.*)?|config)$\"))\n")
}

/// Re-assert the workspace repository's write denials after the scratch
/// allowance. The allowance matches any repository under the scratch path, and
/// a profile can be rendered with the workspace inside it; these two entries
/// land last so the workspace keeps the posture the mandatory deny gives it.
fn deny_workspace_git(workspace: &Path, tag: &str) -> String {
    let ws = workspace.to_string_lossy();
    format!(
        "(deny file-write* (subpath \"{ws}/.git/hooks\") (with message \"{tag}\"))\n\
         (deny file-write* (subpath \"{ws}/.git/config\") (with message \"{tag}\"))\n"
    )
}

/// Escape the characters a seatbelt regex reads as operators, so a scratch
/// path matches itself. The resolved temp path carries dots on this platform
/// and can carry a bracket or a plus on another, and an unescaped one would
/// widen the rule past the directory it names.
fn escape_regex(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(
            c,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
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
