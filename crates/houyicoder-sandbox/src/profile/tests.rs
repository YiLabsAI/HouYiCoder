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
    assert!(p.contains("com.apple.hiservices-xpcservice"));
    assert!(p.contains("(allow ipc-posix-shm)"));
    assert!(p.contains("(allow ipc-posix-sem)"));
    assert!(p.contains("(allow iokit-get-properties)"));
    assert!(p.contains("(allow file-ioctl"));
    assert!(p.contains("(literal \"/dev/null\")"));
    assert!(p.contains("(literal \"/dev/tty\")"));
}

/// Extra mach services declared via ProfileSpec are emitted as
/// standalone allow mach-lookup global-name lines after the base
/// allow_set block. Uses a name not in the base list so the test
/// is independent of which services the base set includes.
#[test]
fn test_extra_mach_services_rendered() {
    let spec = ProfileSpec::new(Path::new("/tmp/ws"), "/tmp", "/Users/test", "tag-x")
        .with_mach_services(&["test.dummy.xpc"]);
    let p = render(&spec);
    assert!(
        p.contains("(allow mach-lookup (global-name \"test.dummy.xpc\"))"),
        "extra mach service emitted: {p}"
    );
}

/// No extra mach services means no standalone allow mach-lookup
/// line for a name outside the base set.
#[test]
fn test_no_extra_mach_services() {
    let spec = ProfileSpec::new(Path::new("/tmp/ws"), "/tmp", "/Users/test", "tag-x");
    let p = render(&spec);
    assert!(
        !p.contains("test.dummy.xpc"),
        "no extra mach service when none declared: {p}"
    );
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
fn test_read_only_dir() {
    let profile = render(
        &ProfileSpec::new(Path::new("/tmp/ws"), "/tmp", "/Users/alice", "tag-x")
            .with_read_only(&["/Users/alice/reference"]),
    );
    let read = profile
        .lines()
        .find(|line| {
            line.starts_with("(allow file-read*") && line.contains("/Users/alice/reference")
        })
        .unwrap();
    let write = profile
        .lines()
        .find(|line| line.starts_with("(allow file-write*"))
        .unwrap();
    assert!(read.contains("/Users/alice/reference"));
    assert!(!write.contains("/Users/alice/reference"));
}

#[test]
fn test_users_open_credentials_denied() {
    // gap B: the /Users broad read-deny is lifted (cross-project source
    // reads must work), and the credential paths otherwise omitted are
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
    // only the named credential paths are.
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

/// The session temporary directory is re-allowed for writes after the
/// mandatory persistence denies, and the permission stores are denied after
/// that so the local permission store stays protected.
#[test]
fn test_tmpdir_write_after_deny() {
    let scratch = "/private/var/folders/T/houyicoder-tmp-1";
    let p = render(&ProfileSpec::new(
        Path::new("/tmp/ws-4"),
        scratch,
        "/Users/bob",
        "tag-s",
    ));
    let hooks_deny = p
        .find("(deny file-write* (subpath \"/Users/bob/.git/hooks\")")
        .expect("hooks deny present");
    let allow = p
        .find(&format!("(allow file-write* (subpath \"{scratch}\"))"))
        .expect("tmpdir allow-back present");
    let auth_deny = p
        .rfind(r".*/houyicoder-permissions/permissions\.json$")
        .expect("permission store deny present");
    assert!(
        allow > hooks_deny && auth_deny > allow,
        "the scratch allow must sit between the persistence denies and the store deny: {p}"
    );
}

/// A settings file under the session scratch directory is writable, while a
/// permission store there is not. The two denies straddle the scratch
/// allow-back for exactly this reason: a sandboxed test lays out its own
/// fixture project in the scratch directory, and a fixture it cannot write
/// reports a permission fault unrelated to what the test exercises.
#[test]
fn test_scratch_settings_file_allowed() {
    let scratch = "/private/var/folders/T/hy-tmp-1";
    let p = render(&ProfileSpec::new(
        Path::new("/tmp/ws-7"),
        scratch,
        "/Users/bob",
        "tag-s",
    ));
    let settings_deny = p
        .find(r".*/\.houyicoder/settings\.json$")
        .expect("settings deny present");
    let allow = p
        .find(&format!("(allow file-write* (subpath \"{scratch}\"))"))
        .expect("tmpdir allow-back present");
    let store_deny = p
        .find(r".*/houyicoder-permissions/permissions\.json$")
        .expect("permission store deny present");
    assert!(
        settings_deny < allow,
        "the settings deny must land before the scratch allow-back, or a \
         fixture project under the scratch directory cannot be written: {p}"
    );
    assert!(
        allow < store_deny,
        "the permission store deny must land after the scratch allow-back, or \
         a local store under it becomes writable: {p}"
    );
}

/// When the temporary directory is empty or contains the workspace, the
/// post-deny allow-back is omitted so the workspace denies stay in force.
#[test]
fn test_tmpdir_nested_omitted() {
    let p_empty = render(&ProfileSpec::new(
        Path::new("/tmp/ws-6"),
        "",
        "/Users/bob",
        "tag-n",
    ));
    assert!(
        !p_empty.contains("(allow file-write* (subpath \"\"))"),
        "no empty subpath allow: {p_empty}"
    );

    let p_nested = render(&ProfileSpec::new(
        Path::new("/tmp/shared/ws"),
        "/tmp/shared/",
        "/Users/bob",
        "tag-w",
    ));
    let hooks_deny = p_nested
        .find("(deny file-write* (subpath \"/Users/bob/.git/hooks\")")
        .expect("hooks deny present");
    let last_allow = p_nested
        .rfind("(allow file-write* (subpath \"/tmp/shared")
        .expect("initial filesystem allow present");
    assert!(
        last_allow < hooks_deny,
        "a tmpdir containing the workspace must not be re-allowed after mandatory_deny: {p_nested}"
    );
}
