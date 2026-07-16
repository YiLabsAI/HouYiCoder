//! Integration tests for the macOS Seatbelt sandbox backend. These run real
//! sandbox-exec commands, so they live outside the unit suite and are gated
//! to macOS. Run with: make test-integration.

#![cfg(target_os = "macos")]

use houyicoder_api::sandbox::SandboxSession;
use houyicoder_sandbox::MacSeatbeltSession;

#[tokio::test]
async fn test_seatbelt_runs_echo() {
    let s = MacSeatbeltSession::new().unwrap();
    let r = s.exec("echo hello").await.unwrap();
    assert_eq!(r.stdout.trim(), "hello");
    assert!(r.is_success(), "stderr: {}", r.stderr);
}

#[tokio::test]
async fn test_seatbelt_writes() {
    let s = MacSeatbeltSession::new().unwrap();
    let r = s.exec("printf 'world' > out.txt").await.unwrap();
    assert!(r.is_success(), "stderr: {}", r.stderr);
    let bytes = s.read_file("out.txt", 64).await.unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "world");
    assert!(s.path_exists("out.txt").await.unwrap());
}

#[tokio::test]
async fn test_seatbelt_denies_escape() {
    // Writing to /etc must be refused by the kernel fence. Assert by file
    // existence (the probe must NOT be created) rather than by the shell's
    // exit code: zsh inside the sandbox's eval wrapper does not reliably set
    // a non-zero $? on a redirect failure, so checking stdout for "exit=1"
    // is fragile. The security property is "the file was not created".
    let s = MacSeatbeltSession::new().unwrap();
    let probe = "/etc/houyicoder-escape-probe";
    std::fs::remove_file(probe).ok();
    s.exec(&format!("printf x > {probe}")).await.ok();
    assert!(
        !std::path::Path::new(probe).exists(),
        "the fence must refuse the write to /etc — the probe file must not exist"
    );
    // Positive control: a write INSIDE the workspace succeeds (the deny is
    // path-scoped, not a blanket write-deny). Without this the refuse is
    // indistinguishable from writes never working at all.
    let inside = s.exec("printf ok > escape-probe-inside").await.unwrap();
    assert!(
        inside.is_success(),
        "a workspace write must succeed (positive control): {inside:?}"
    );
    assert!(
        s.path_exists("escape-probe-inside").await.unwrap(),
        "the workspace file must exist (positive control)"
    );
}

#[tokio::test]
async fn test_list_dir_path() {
    let s = MacSeatbeltSession::new().unwrap();
    s.exec("mkdir -p sub && printf a > sub/f").await.unwrap();
    let entries = s.list_dir("sub").await.unwrap();
    assert!(entries.iter().any(|e| e.name == "f"));
    assert!(s.path_exists("sub/f").await.unwrap());
    assert!(!s.path_exists("nope").await.unwrap());
}
