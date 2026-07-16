//! Seatbelt profile builders — composable segments composing into a full
//! macOS sandbox-exec profile. Industrial-grade allowlist: deny default +
//! an explicit allow-set covering everything sh/dyld needs to start, then
//! allow-only file writes (workspace + tmpdir + stdio), a network segment that
//! denies egress by default,
//! and layer mandatory deny + denyReadAlways on top so AI-agent
//! exfiltration vectors (.ssh, .gitconfig, .mcp.json, .git/hooks, ...) cannot
//! be read or written even when nested inside an allowed path.
//!
//! The segments encode: the mach services and /dev devices dyld touches
//! during process start, the fs three-stage shape (allow-all -> deny-broad
//! -> allow-back), the denyReadAlways re-emit rule, and the mandatory deny
//! file/dir list. Each segment is an independent Rust builder, unit-testable
//! in isolation.

use houyicoder_api::sandbox::NetworkPolicy;
use std::path::Path;

mod deny;
mod network;

use self::deny::{deny_snapshot_store, mandatory_deny};
use self::network::network_rules;

/// mach services dyld/launchd look up during process start (srt's set).
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

/// Broad read-deny regions. Only /etc remains: system config that is
/// re-allow-listed back for the specific files git must read (see
/// SYSTEM_ETC_ALLOW). The home tree (/Users) is NOT a broad deny anymore —
/// that posture blocked legitimate cross-project source reads (a workspace
/// under /Users could read itself but not sibling projects), which is
/// stricter than a read-all stance without buying extra safety (writes
/// still default-deny to workspace+additional). Sensitive home
/// paths are denied by name in mandatory_deny (.ssh, .aws, .gnupg, .bashrc,
/// .mcp.json, .netrc, ...), so lifting the broad /Users deny opens ordinary
/// source reads while the credential vectors stay denied. A user can widen
/// reads further via the read-allowlist config (additional read-allow paths).
const DENY_REGIONS: &[&str] = &["/etc", "/private/etc"];

/// System-wide files under /etc that git must read at startup. The shell runs
/// non-login (/bin/sh -c), so it does not source /etc/profile; /etc/gitconfig
/// is the system-level git config every git invocation reads, and /etc/paths is
/// read by path_helper. The broad /etc deny (DENY_REGIONS) and the mandatory
/// .profile/.gitconfig regex denies block these reads, so git would print
/// "fatal: unable to access '/etc/gitconfig'" — which surfaced on Edit chips
/// because a same-turn Bash+git result got misattached (bug-log #17). These
/// are SYSTEM config (no credentials; ~/.gitconfig and ~/.profile stay
/// denied), so reads are allow-listed back after the mandatory denies so
/// last-match-wins lets them through.
///
/// /etc/profile is listed for completeness even though the non-login shell no
/// longer sources it: a future tool that reads it directly stays allowed, and
/// the entry is harmless while the shell does not source it.
///
/// Listed as the /etc form only; allow_system_etc also emits the /private/etc
/// resolved form, because on macOS /etc is a symlink to /private/etc and
/// seatbelt matches the resolved realpath. Adding a new system file here
/// auto-covers both paths — no manual /private/etc mirror per entry.
const SYSTEM_ETC_ALLOW: &[&str] = &[
    "/etc/profile",
    "/etc/gitconfig",
    "/etc/paths",
    // cargo links the system OpenSSL/LibreSSL, which reads this public
    // trust-store config at process init; denying it breaks cargo.
    "/etc/ssl/openssl.cnf",
];

/// Render a full Seatbelt profile bound to the workspace. Composes six
/// segments in order: deny_default -> allow_set -> filesystem_rules ->
/// deny_read_always -> mandatory_deny -> network. Order is load-bearing:
/// seatbelt is last-match-wins, so every deny that must hold against an
/// allow-back has to land after the allow-back segment.
pub fn render_profile(workspace: &Path, tmpdir: &str, tag: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
    render(&ProfileSpec::new(workspace, tmpdir, &home, tag))
}

/// Everything the profile renderer needs, as one named value.
///
/// A struct rather than a positional parameter list because three of the four
/// required inputs are strings and two of them are paths: transposing the
/// temp dir, the home dir, and the violation tag compiles cleanly and produces a
/// fence that is wrong in a way no test would obviously catch. Naming them at
/// the call site removes that class of mistake. Marked non-exhaustive so a later
/// fence axis is an added builder method rather than a break at every call site.
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
    /// Extra directories the user added to the workspace at runtime. They land
    /// in the fs allow-back alongside the workspace and temp dir so reads and
    /// writes pass, while the mandatory exfiltration denies that follow still
    /// hold inside them.
    pub additional: &'a [&'a str],
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
            additional: &[],
            network: NetworkPolicy::contained(),
        }
    }

    /// Set the runtime-added workspace directories.
    #[must_use]
    pub fn with_additional(mut self, additional: &'a [&'a str]) -> Self {
        self.additional = additional;
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
        network,
    } = spec;
    let (tmpdir, home, tag) = (*tmpdir, *home, *tag);
    let mut s = String::new();
    s.push_str("(version 1)\n");
    s.push_str(&deny_default(tag));
    s.push_str(&allow_set(tag));
    s.push_str(&filesystem_rules(workspace, tmpdir, tag, additional));
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
        v.extend(additional.iter().copied());
        v
    };
    s.push_str(&deny_read_always(DENY_REGIONS, &allow_paths, tag));
    // mandatory_deny is the concrete application of denyReadAlways to the
    // exfiltration vectors: it lands after the allow-back so an allow-back
    // of the workspace cannot wash out a deny of .ssh/.gitconfig inside it.
    s.push_str(&mandatory_deny(home, tag));
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
/// The alternative of projecting a minimal config through the environment was
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
/// mach-lookup of 15 system services, ipc-posix-shm/sem, iokit,
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

/// Filesystem three-stage (srt macos-sandbox-utils:225-308): allow-all read
/// -> deny broad regions -> re-allow workspace+tmpdir. Last-match-wins so
/// workspace reads pass while /Users at large stays denied. Writes are
/// allow-only (workspace + tmpdir + stdio devices) — there is no
/// write-everything default.
pub fn filesystem_rules(workspace: &Path, tmpdir: &str, tag: &str, additional: &[&str]) -> String {
    let ws = workspace.to_string_lossy();
    let deny_regions = DENY_REGIONS
        .iter()
        .map(|r| format!("(subpath \"{r}\")"))
        .collect::<Vec<_>>()
        .join(" ");
    // The allow-back subpaths: workspace + tmpdir + any user-added working
    // directories. Each additional dir is re-allowed here (after the broad
    // deny of /Users etc.) so last-match-wins lets reads through; the
    // mandatory exfiltration denies that follow still hold inside them.
    let mut allow_subs = format!("(subpath \"{ws}\") (subpath \"{tmpdir}\")");
    for d in additional {
        if d.is_empty() {
            continue;
        }
        allow_subs.push_str(&format!(" (subpath \"{d}\")"));
    }
    format!(
        "(allow file-read*)\n\
         (deny file-read* {deny_regions} (with message \"{tag}\"))\n\
         (allow file-read* {allow_subs})\n\
         (allow file-write* {allow_subs} (literal \"/dev/null\") (literal \"/dev/stdout\") (literal \"/dev/stderr\"))\n"
    )
}

/// denyReadAlways (srt macos-sandbox-utils:304-311): for each literal deny
/// path that is nested inside an allow-back subpath, re-emit the deny after
/// the allow-back so last-match-wins does not wash it out. Returns the empty
/// string when no deny path is nested inside an allow path (the common case
/// when the workspace lives in tmpdir, not under /Users). The builder stays
/// parameterized so a richer configuration model can drive non-trivial
/// allow-back sets without changing the render pipeline.
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
mod tests {
    use super::*;

    #[test]
    fn test_deny_default_emits_tag() {
        let p = deny_default("tag-x");
        assert_eq!(p, "(deny default (with message \"tag-x\"))\n");
    }

    #[test]
    fn test_allow_set_mach_services() {
        let p = allow_set("tag-x");
        assert!(p.contains("(allow process-exec)"));
        assert!(p.contains("(allow mach-lookup"));
        assert!(p.contains("com.apple.system.logger"));
        assert!(p.contains("com.apple.SecurityServer"));
        assert!(p.contains("(allow ipc-posix-shm)"));
        assert!(p.contains("(allow ipc-posix-sem)"));
        assert!(p.contains("(allow iokit-get-properties)"));
        assert!(p.contains("(allow file-ioctl"));
        assert!(p.contains("(literal \"/dev/null\")"));
        assert!(p.contains("(literal \"/dev/tty\")"));
    }

    #[test]
    fn test_filesystem_rules_order() {
        let p = filesystem_rules(Path::new("/tmp/ws-1"), "/tmp", "tag-x", &[]);
        assert!(p.contains("(allow file-read*)\n"));
        assert!(p.contains("(deny file-read*"));
        // /Users is intentionally NOT a broad deny (cross-project source reads
        // must work; sensitive home paths are denied by name in mandatory_deny).
        assert!(!p.contains("(subpath \"/Users\")"));
        assert!(p.contains("(subpath \"/etc\")"));
        assert!(p.contains("(subpath \"/private/etc\")"));
        assert!(p.contains("(allow file-read* (subpath \"/tmp/ws-1\") (subpath \"/tmp\"))"));
        assert!(p.contains("(allow file-write* (subpath \"/tmp/ws-1\")"));
        assert!(p.contains("(literal \"/dev/stdout\")"));
    }

    #[test]
    fn test_profile_allows_extra_dirs() {
        // A user-added dir under /Users is allow-backed for both reads and
        // writes. Last-match-wins lets it through; the mandatory denies that
        // follow still hold inside it. (The /Users broad read-deny was lifted
        // so cross-project source reads work; this allow-back now mainly
        // serves writes to user-added dirs, which the default-deny write
        // posture would otherwise refuse.)
        let p = filesystem_rules(
            Path::new("/tmp/ws-1"),
            "/tmp",
            "tag-x",
            &["/Users/alice/projects/foo"],
        );
        assert!(
            p.contains("(subpath \"/Users/alice/projects/foo\")"),
            "additional dir lands in the allow-back: {p}"
        );
        let read_back = p
            .find("(allow file-read*")
            .filter(|i| p[*i..].contains("/Users/alice/projects/foo"))
            .expect("additional dir is in the read allow-back");
        let write_back = p
            .find("(allow file-write*")
            .filter(|i| p[*i..].contains("/Users/alice/projects/foo"))
            .expect("additional dir is in the write allow-back");
        assert!(write_back > read_back);
    }

    #[test]
    fn test_users_open_credentials_denied() {
        // gap B: the /Users broad read-deny is lifted (cross-project source
        // reads must work), and the credential vectors otherwise omitted are
        // denied by name in mandatory_deny so lifting /Users does not expose
        // them.
        let p = render(&ProfileSpec::new(
            Path::new("/tmp/ws-1"),
            "/tmp",
            "/Users/alice",
            "tag-x",
        ));
        assert!(
            !p.contains("(deny file-read* (subpath \"/Users\")"),
            "no broad /Users read-deny (cross-project reads open): {p}"
        );
        // Credential dirs/files added beyond the base dangerous list.
        for cred in [".aws", ".gnupg", ".kube", ".docker"] {
            assert!(
                p.contains(&format!("(subpath \"/Users/alice/{cred}\")")),
                "credential dir {cred} in mandatory deny: {p}"
            );
        }
        for cred in [".netrc", ".npmrc", ".pypirc"] {
            assert!(
                p.contains(&format!("(subpath \"/Users/alice/{cred}\")")),
                "credential file {cred} in mandatory deny: {p}"
            );
        }
        // Ordinary source under /Users (a sibling project) is NOT denied —
        // only the named credential vectors are.
        assert!(
            !p.contains("(subpath \"/Users/alice/projects\")"),
            "ordinary sibling project path is not denied: {p}"
        );
    }

    #[test]
    fn test_nested_deny_survives_allow() {
        // /Users/me/secrets is nested inside the allow-back path /Users/me
        // — deny must be re-emit so the allow-back cannot wash it out.
        let p = deny_read_always(&["/Users/me/secrets"], &["/Users/me"], "tag-x");
        assert!(p.contains("(deny file-read* (subpath \"/Users/me/secrets\")"));
        assert!(p.contains("(with message \"tag-x\")"));
    }

    #[test]
    fn test_deny_read_always_empty() {
        // /Users is not inside /tmp/ws-1 — nothing re-emit.
        let p = deny_read_always(&["/Users"], &["/tmp/ws-1"], "tag-x");
        assert_eq!(p, "");
    }

    #[test]
    fn test_allows_system_etc_resolved() {
        // /etc is a symlink to /private/etc on macOS; seatbelt matches the
        // resolved realpath, so the allow must cover both forms or the login
        // shell's /etc/profile read stays denied and pollutes bash stderr.
        let p = allow_system_etc("tag-x");
        assert!(p.contains("(literal \"/etc/profile\")"));
        assert!(p.contains("(literal \"/private/etc/profile\")"));
        assert!(p.contains("(literal \"/etc/gitconfig\")"));
        assert!(p.contains("(literal \"/private/etc/gitconfig\")"));
    }

    #[test]
    fn test_render_profile_contains_mandatory() {
        let p = render(&ProfileSpec::new(
            Path::new("/tmp/ws-2"),
            "/tmp",
            "/Users/bob",
            "tag-z",
        ));
        assert!(p.contains("(deny default (with message \"tag-z\"))"));
        assert!(p.contains("(allow process-exec)"));
        assert!(p.contains("(subpath \"/Users/bob/.ssh\")"));
        assert!(p.contains("(subpath \"/Users/bob/.gitconfig\")"));
        assert!(p.contains("(deny network* (with message \"tag-z\"))"));
        // mandatory deny must land after the workspace allow-back so it
        // cannot be washed out.
        let allow_back = p.find("(allow file-read* (subpath \"/tmp/ws-2\")").unwrap();
        let mandatory = p.find("(subpath \"/Users/bob/.ssh\")").unwrap();
        assert!(mandatory > allow_back);
    }

    /// System /etc files git reads at startup must be re-allowed AFTER the
    /// broad /etc deny and the mandatory rc-file regex denies, so git can read
    /// the system git config. Without this, git prints that it cannot access
    /// that path. The shell is non-login so it does not source /etc/profile.
    /// The home profile file stays fully denied; the home git config is a
    /// deliberate exception covered by the two tests below.
    #[test]
    fn test_etc_allow_after_deny() {
        let p = render(&ProfileSpec::new(
            Path::new("/tmp/ws-2"),
            "/tmp",
            "/Users/bob",
            "tag-z",
        ));
        // The system /etc allow-back is present, covering BOTH the /etc
        // literal and the /private/etc resolved form (seatbelt matches the
        // realpath on macOS where /etc -> /private/etc).
        assert!(
            p.contains("(allow file-read* (literal \"/etc/profile\")"),
            "system /etc allow-back missing: {p}"
        );
        assert!(
            p.contains("(literal \"/private/etc/profile\")"),
            "resolved /private/etc form missing: {p}"
        );
        // And it lands AFTER the mandatory deny (last-match-wins re-allows).
        let mandatory_gitconfig = p
            .find("(subpath \"/Users/bob/.gitconfig\")")
            .expect("mandatory ~/.gitconfig deny present");
        let sys_allow = p
            .find("(literal \"/etc/gitconfig\")")
            .expect("system /etc/gitconfig allow present");
        assert!(
            sys_allow > mandatory_gitconfig,
            "system /etc allow must land after the mandatory deny (last-match-wins)"
        );
        // The home profile file stays fully denied — the system copy being
        // re-allowed must not leak into the home one.
        assert!(
            p.contains("(deny file-read* file-write* (subpath \"/Users/bob/.profile\")"),
            "home profile file must stay denied: {p}"
        );
        // Read-only: the system allow must NOT grant writes.
        assert!(
            !p.contains("(allow file-write* (literal \"/etc/gitconfig\")"),
            "system /etc files must be read-only, not writable: {p}"
        );
    }

    /// The etc allow-back must also cover the etc node itself. On macOS that
    /// node is a symlink, and the broad deny of the etc subpath covers it, so
    /// the kernel refuses while resolving the path and never reaches the leaf
    /// rule. Without this line every leaf allow in the segment is dead code, a
    /// state a real-kernel probe caught and no string assertion could.
    #[test]
    fn test_etc_covers_link() {
        let p = allow_system_etc("tag-x");
        assert!(
            p.contains("(allow file-read-metadata (literal \"/etc\"))"),
            "the etc link node must be resolvable or the leaf allows are dead: {p}"
        );
    }

    /// The home git config read allow-back must land after the mandatory deny
    /// segment. Seatbelt is last-match-wins, so an allow emitted before the
    /// deny would be washed out and git would fail again.
    #[test]
    fn test_gitconfig_read_reallowed() {
        let p = render(&ProfileSpec::new(
            Path::new("/tmp/ws-3"),
            "/tmp",
            "/Users/bob",
            "tag-z",
        ));
        let deny = p
            .find("(deny file-write* (subpath \"/Users/bob/.gitconfig\")")
            .expect("write-only deny present");
        let allow = p
            .find("(allow file-read* (literal \"/Users/bob/.gitconfig\"))")
            .expect("read allow-back present");
        assert!(
            allow > deny,
            "the read allow-back must land after the deny segment: {p}"
        );
    }
}
