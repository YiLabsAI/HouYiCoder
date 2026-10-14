//! Effect-level fence test for the repo git-config read posture. The
//! profile must let git log read .git/config: the mandatory .git/config
//! rule is write-only (read allowed, write denied), so daily git (log,
//! diff, status) works while credential-helper injection via config
//! writes stays blocked. Main-repo counterpart to the worktree spike: no
//! worktree, no manual .git/config read-allow append, so a read deny here
//! makes git log fatal. Runs real sandbox-exec; ignored (too slow for
//! the commit gate).

#![allow(clippy::disallowed_methods)]

use super::super::*;
use std::env::temp_dir;
use std::env::var;
use std::fs::create_dir_all;
use std::fs::remove_dir_all;
use std::process::Command;
use std::process::id;
use std::sync::atomic::Ordering;

/// The fence lets git log read .git/config. If .git/config is
/// read-denied, git log fatals. Pins the write-only config posture.
#[tokio::test]
#[ignore]
async fn test_git_config_read_works() {
    let parent = temp_dir().join(format!(
        "houyi-mainrepo-{}-{}",
        id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    create_dir_all(&parent).expect("mkdir parent");
    let repo = parent.join("repo");
    create_dir_all(&repo).expect("mkdir repo");
    let run = |args: &[&str]| Command::new("git").arg("-C").arg(&repo).args(args).status();
    assert!(run(&["init", "-q"]).unwrap().success(), "git init");
    let _ = run(&["config", "user.email", "spike@x"]).unwrap();
    let _ = run(&["config", "user.name", "spike"]).unwrap();
    assert!(
        run(&["commit", "--allow-empty", "-m", "init", "-q"])
            .unwrap()
            .success(),
        "initial commit so HEAD exists"
    );
    // Default (non-worktree) profile: fence is the repo, no manual
    // .git/config read-allow. A read deny on .git/config makes git log
    // fatal here.
    let tmpdir = var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let home = var("HOME").unwrap_or_else(|_| "/Users/unknown".into());
    let stag = format!("houyi-mainrepo-{}", id());
    let profile = render(&ProfileSpec::new(&repo, &tmpdir, &home, &stag));
    let out = Command::new("sandbox-exec")
        .arg("-p")
        .arg(&profile)
        .arg("--")
        .arg("/bin/sh")
        .arg("-c")
        .arg("git -c user.email=spike@x -c user.name=spike log --oneline -1")
        .current_dir(&repo)
        .output()
        .expect("spawn sandbox-exec");
    assert!(
        out.status.success(),
        "git log must read .git/config under the default profile; .git/config deny must be write-only not read+write: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("init"),
        "log must show the init commit: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    remove_dir_all(&parent).ok();
}
