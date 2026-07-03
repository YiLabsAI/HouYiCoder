//! Tests for the sandbox network policy loader.
//!
//! The load-bearing property is the direction of every fallback. This is a
//! containment knob, so an absent file, a corrupt file, or an unrecognised mode
//! must all land on the default posture (Open, gated per command) rather than
//! widening beyond it. A loader that widened the fence on malformed input would
//! turn a typo into an egress hole, so each failure mode gets its own test
//! rather than being folded into one.

use super::*;

fn write_settings(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("settings.json");
    std::fs::write(&path, body).expect("write settings fixture");
    path
}

fn tempdir() -> std::path::PathBuf {
    let base = std::env::temp_dir().join(format!(
        "hc-net-cfg-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&base).expect("create fixture dir");
    base
}

#[test]
fn test_default_egress_open_gated() {
    // The default is Open so ordinary development traffic (git push, package
    // fetches) works; the gate asks before an egress command runs, so the
    // per-command consent point is the control rather than a blanket kernel
    // deny. Off is the opt-in for users who want kernel-level no-network.
    let c = SandboxNetworkConfig::default();
    assert_eq!(c.mode, NetworkMode::Open);
    assert!(c.allow_unix_sockets.is_empty());
    assert!(!c.allow_all_unix_sockets);
    assert!(!c.allow_local_binding);
}

#[test]
fn test_missing_settings_uses_default() {
    let dir = tempdir();
    let path = dir.join("absent-settings.json");
    assert_eq!(
        load_sandbox_network_from(&path),
        SandboxNetworkConfig::default()
    );
}

#[test]
fn test_corrupt_settings_uses_default() {
    let dir = tempdir();
    let path = write_settings(&dir, "{ this is not json");
    let c = load_sandbox_network_from(&path);
    assert_eq!(
        c.mode,
        NetworkMode::Open,
        "corrupt settings must fall back to the open default, not silently close"
    );
    assert!(c.allow_unix_sockets.is_empty());
    assert!(!c.allow_all_unix_sockets);
    assert!(!c.allow_local_binding);
}

#[test]
fn test_unknown_mode_reported() {
    // An unrecognised mode must not open the fence, and must not be silent
    // either. The proxy mode is the realistic case: the design documents it as
    // planned, so a user may well write it before it exists, and a fence that
    // closes with no explanation would read as a bug.
    let dir = tempdir();
    let path = write_settings(&dir, r#"{"sandbox":{"network":{"mode":"proxy"}}}"#);
    let c = load_sandbox_network_from(&path);
    assert_eq!(c.mode, NetworkMode::Unsupported("proxy".into()));
    let w = c.deferred_warnings();
    assert_eq!(w.len(), 1, "got {w:?}");
    assert!(w[0].contains("proxy"), "the warning names the value: {w:?}");
}

#[test]
fn test_unknown_mode_keeps_siblings() {
    // The whole section used to fail the parse on a bad mode, discarding the
    // sibling keys with it. Holding the value keeps the rest readable.
    let dir = tempdir();
    let path = write_settings(
        &dir,
        r#"{"sandbox":{"network":{"mode":"proxy","allow_local_binding":true}}}"#,
    );
    let c = load_sandbox_network_from(&path);
    assert!(c.allow_local_binding, "the sibling key survives");
}

#[test]
fn test_open_mode_parses() {
    let dir = tempdir();
    let path = write_settings(&dir, r#"{"sandbox":{"network":{"mode":"open"}}}"#);
    assert_eq!(load_sandbox_network_from(&path).mode, NetworkMode::Open);
}

#[test]
fn test_ipc_fields_parse() {
    // Local IPC fields must not flip the egress posture: with mode explicitly
    // off, setting unix-socket and loopback allow-backs keeps egress off (they
    // are local IPC, not egress). The default is open, so the test pins the
    // no-flip against an explicit off rather than against the default.
    let dir = tempdir();
    let path = write_settings(
        &dir,
        r#"{"sandbox":{"network":{
             "mode":"off",
             "allow_unix_sockets":["/tmp/agent.sock"],
             "allow_local_binding":true
           }}}"#,
    );
    let c = load_sandbox_network_from(&path);
    assert_eq!(c.mode, NetworkMode::Off, "local IPC must not imply egress");
    assert_eq!(c.allow_unix_sockets, vec!["/tmp/agent.sock".to_string()]);
    assert!(c.allow_local_binding);
}

#[test]
fn test_unrelated_keys_ignored() {
    // The loader owns one path in a shared settings file; keys belonging to
    // other sections must not make it fail and fall back.
    let dir = tempdir();
    let path = write_settings(
        &dir,
        r#"{"auto_memory":false,"sandbox":{"network":{"mode":"open"},"enabled":true}}"#,
    );
    assert_eq!(load_sandbox_network_from(&path).mode, NetworkMode::Open);
}

#[test]
fn test_absent_denies_egress() {
    let dir = tempdir();
    let path = write_settings(&dir, r#"{"auto_memory":true}"#);
    assert_eq!(
        load_sandbox_network_from(&path),
        SandboxNetworkConfig::default()
    );
}

#[test]
fn test_deferred_keys_reported() {
    // A destination allowlist this version cannot enforce must be reported. Left
    // silent it is worse than unsupported: the user writes an allowlist, is told
    // nothing, and believes it is restricting where the agent can reach.
    let dir = tempdir();
    let path = write_settings(
        &dir,
        r#"{"sandbox":{"network":{"mode":"off","allow":["corp.example"],"unknown_domain":"ask"}}}"#,
    );
    let c = load_sandbox_network_from(&path);
    let w = c.deferred_warnings();
    assert_eq!(w.len(), 2, "both keys reported, got {w:?}");
    assert!(w.iter().any(|m| m.contains("allow")));
    assert!(w.iter().any(|m| m.contains("unknown_domain")));
    assert!(
        w.iter().all(|m| m.contains("not in effect yet")),
        "a planned key reads as not-yet, not as a typo: {w:?}"
    );
    assert_eq!(c.mode, NetworkMode::Off, "an unenforced key stays narrow");
}

#[test]
fn test_unknown_key_reported() {
    // A misspelling would otherwise be silent, leaving the user with a fence
    // narrower than they configured and no clue why.
    let dir = tempdir();
    let path = write_settings(
        &dir,
        r#"{"sandbox":{"network":{"allow_local_bindings":true}}}"#,
    );
    let c = load_sandbox_network_from(&path);
    assert!(!c.allow_local_binding, "the misspelled key sets nothing");
    let w = c.deferred_warnings();
    assert_eq!(w.len(), 1);
    assert!(w[0].contains("unknown key"), "got {w:?}");
}

#[test]
fn test_acted_keys_stay_quiet() {
    let dir = tempdir();
    let path = write_settings(
        &dir,
        r#"{"sandbox":{"network":{"mode":"open","allow_local_binding":true}}}"#,
    );
    assert!(
        load_sandbox_network_from(&path)
            .deferred_warnings()
            .is_empty()
    );
}
