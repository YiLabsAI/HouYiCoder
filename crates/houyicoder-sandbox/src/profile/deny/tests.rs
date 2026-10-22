use super::*;

#[test]
fn test_mandatory_deny_home_paths() {
    let p = mandatory_deny("/Users/alice", "tag-x");
    assert!(p.contains("(subpath \"/Users/alice/.ssh\")"));
    assert!(p.contains("(subpath \"/Users/alice/.gitconfig\")"));
    assert!(p.contains("(subpath \"/Users/alice/.mcp.json\")"));
    assert!(p.contains("(subpath \"/Users/alice/.bashrc\")"));
    assert!(p.contains("(subpath \"/Users/alice/.zshrc\")"));
    assert!(p.contains("(subpath \"/Users/alice/.profile\")"));
    assert!(p.contains("(subpath \"/Users/alice/.claude/commands\")"));
    assert!(p.contains("(subpath \"/Users/alice/.claude/agents\")"));
}

/// The ecosystem-compat and agents skill dirs stay read+write denied:
/// those sources get no resource layer, and the read-deny blocks direct
/// reads of their skill bodies. The native skills dir is write-only:
/// reads are allowed back (the resource layer) while writes stay
/// denied so the agent cannot plant a SKILL.md. The body reaches the
/// model via the host-side load, so the read-deny added nothing.
#[test]
fn test_skill_dirs_denied() {
    let p = mandatory_deny("/Users/alice", "tag-x");
    // Ecosystem + agents dirs: read+write denied (no resource layer).
    for dir in [".claude/skills", ".agents/skills"] {
        assert!(
            p.contains(&format!(
                "(deny file-read* file-write* (subpath \"/Users/alice/{dir}\")"
            )),
            "{dir} must be read+write denied: {p}"
        );
    }
    // Native skills dir: write-only (reads allowed back, writes denied).
    assert!(
        p.contains("(deny file-write* (subpath \"/Users/alice/.houyicoder/skills\")"),
        "native skills dir must be write-denied: {p}"
    );
    assert!(
        !p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.houyicoder/skills\")"),
        "native skills dir must not be read-denied (resource layer): {p}"
    );
    // The write-deny regex covers the nested-anywhere form so it holds
    // for the user-level path and the project path, not only the home
    // literal — a SKILL.md planted anywhere under the native skills
    // path is blocked, not just the home copy.
    assert!(
        p.contains(r".*/\.houyicoder/skills(/|$)"),
        "native skills dir write-deny covers nested-anywhere: {p}"
    );
}

#[test]
fn test_git_hooks_write_denied() {
    let p = mandatory_deny("/Users/alice", "tag-x");
    assert!(p.contains("(subpath \"/Users/alice/.git/hooks\")"));
    assert!(p.contains(r".*/\.git/hooks(/|$)"));
    assert!(
        p.contains("(deny file-write* (subpath \"/Users/alice/.git/hooks\")"),
        "hooks must be write-denied: {p}"
    );
    assert!(
        !p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.git/hooks\")"),
        "hooks must not be read-denied, same posture as git config: {p}"
    );
}

#[test]
fn test_permission_and_settings_denied() {
    // The profile places the two groups on either side of the session scratch
    // allow-back, so both are asserted through their own emitters rather than
    // through mandatory_deny, which no longer carries them.
    let p = format!(
        "{}{}",
        deny_settings_files("/Users/alice", "tag-x"),
        deny_permission_stores("/Users/alice", "tag-x")
    );
    assert!(p.contains(r".*/\.houyicoder/permissions\.json$"));
    assert!(p.contains(r".*/\.houyicoder/settings\.json$"));
    assert!(p.contains(r".*/\.houyicoder/skill-grants\.json$"));
    assert!(p.contains(r".*/houyicoder-permissions/permissions\.json$"));
    assert!(p.contains("(subpath \"/Users/alice/.houyicoder/permissions.json\")"));
    let line = p
        .lines()
        .find(|l| l.contains(r".*/\.houyicoder/permissions\.json$"))
        .expect("permission store deny present");
    assert!(
        line.starts_with("(deny file-write*"),
        "these files are write-denied only: authoring rules is the hazard, \
         reading them is not: {line}"
    );
}

/// The guard must not widen into a directory deny. Worktrees are created
/// under the same directory, so a session can be working inside it, and a
/// deny written against the directory would refuse the agent every write in
/// its own workspace.
#[test]
fn test_store_deny_stays_narrow() {
    let p = mandatory_deny("/Users/alice", "tag-x");
    assert!(
        !p.contains(r".*/\.houyicoder(/|$)"),
        "a directory-level deny of the config directory breaks worktree \
         sessions, which run with their workspace inside it"
    );
}

#[test]
fn test_mandatory_denies_read_write() {
    let p = mandatory_deny("/Users/alice", "tag-x");
    assert!(p.contains("deny file-read* file-write*"));
}

/// The home git config is denied for write only. Read is allowed back
/// further down so git works; write stays denied because a writable git
/// config is a credential-helper injection vector. Every other mandatory
/// entry keeps denying both verbs.
#[test]
fn test_gitconfig_write_denied() {
    let p = mandatory_deny("/Users/alice", "tag-x");
    assert!(
        p.contains("(deny file-write* (subpath \"/Users/alice/.gitconfig\")"),
        "home git config must be write-denied: {p}"
    );
    assert!(
        !p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.gitconfig\")"),
        "home git config must not be read-denied, the read allow-back depends on it: {p}"
    );
    assert!(
        p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.ssh\")"),
        "other mandatory entries must still deny both verbs: {p}"
    );
}

/// The repo .git/config is denied for write only, same posture as the
/// home git config above: git reads it on every log/diff/status, so a
/// read deny breaks daily git; write stays denied because a writable
/// config is a credential-helper injection vector.
#[test]
fn test_git_config_write_denied() {
    let p = mandatory_deny("/Users/alice", "tag-x");
    assert!(
        p.contains("(deny file-write* (subpath \"/Users/alice/.git/config\")"),
        "repo .git/config must be write-denied: {p}"
    );
    assert!(
        !p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.git/config\")"),
        "repo .git/config must not be read-denied, git needs to read it: {p}"
    );
}

/// .gitmodules is denied for write only: its contents are submodule
/// URLs (not secrets), so reads are safe, and the danger is planting a
/// malicious submodule URL that fires on the next submodule update.
#[test]
fn test_gitmodules_write_denied() {
    let p = mandatory_deny("/Users/alice", "tag-x");
    assert!(
        p.contains("(deny file-write* (subpath \"/Users/alice/.gitmodules\")"),
        ".gitmodules must be write-denied: {p}"
    );
    assert!(
        !p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.gitmodules\")"),
        ".gitmodules must not be read-denied, submodule URLs are not secrets: {p}"
    );
}

/// .ripgreprc is denied for write only: rg reads it on startup, and its
/// contents are rg flags/themes (no secrets), so reads are safe. Write
/// stays denied to prevent planting malicious rg flags.
#[test]
fn test_ripgreprc_write_denied() {
    let p = mandatory_deny("/Users/alice", "tag-x");
    assert!(
        p.contains("(deny file-write* (subpath \"/Users/alice/.ripgreprc\")"),
        ".ripgreprc must be write-denied: {p}"
    );
    assert!(
        !p.contains("(deny file-read* file-write* (subpath \"/Users/alice/.ripgreprc\")"),
        ".ripgreprc must not be read-denied, rg needs to read it and it has no secrets: {p}"
    );
}

/// The user-wide snapshot store carries a write deny: it holds the undo data
/// for destructive commands, so a command that could write it could rewrite its
/// own recovery evidence.
#[test]
fn test_home_store_denied() {
    let p = deny_home_snapshot_store("/Users/alice", "tag-x");
    assert!(
        p.contains("(deny file-write* (subpath \"/Users/alice/.houyicoder/snapshots\")"),
        "the home snapshot store must be write-denied: {p}"
    );
    assert!(
        !p.contains("file-read"),
        "reading the store is not the hazard; only writing it is: {p}"
    );
}
