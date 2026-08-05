//! Tests for the settings-to-policy mapping.
//!
//! Two properties carry weight. Every rejection must narrow the fence rather
//! than widen it, and every rejection must be reported, because an unreported
//! one surfaces later as an unexplained failure from an unrelated tool.

use super::*;

fn settings() -> SandboxNetworkConfig {
    SandboxNetworkConfig::default()
}

#[test]
fn test_default_maps_open() {
    // The default posture is open (aligned with the default agent): the
    // gate asks per egress command, so the open fence plus per-command ask
    // is the control, not a blanket kernel deny. Contained is the opt-in
    // for users who set mode off.
    let (p, w) = network_policy_from(&settings());
    assert!(p.allows_egress(), "default posture is open");
    assert!(w.is_empty(), "the default posture warrants no warning");
}

#[test]
fn test_open_grants_egress() {
    let mut c = settings();
    c.mode = NetworkMode::Open;
    let (p, w) = network_policy_from(&c);
    assert!(p.allows_egress());
    assert!(w.is_empty());
}

#[test]
fn test_binding_carries_over() {
    // Pin an explicit off so the assertion isolates "local binding does
    // not grant egress" — under the default open posture, egress is open
    // by the default, not by binding, so the no-widen claim only reads
    // against an explicit off.
    let mut c = settings();
    c.mode = NetworkMode::Off;
    c.allow_local_binding = true;
    let (p, w) = network_policy_from(&c);
    assert!(p.allow_local_binding);
    assert!(!p.allows_egress(), "local binding is not egress");
    assert!(w.is_empty());
}

#[test]
fn test_list_maps_paths() {
    let mut c = settings();
    c.allow_unix_sockets = vec!["/tmp/a.sock".into()];
    let (p, w) = network_policy_from(&c);
    assert_eq!(
        p.unix_sockets,
        UnixSockets::Paths(vec!["/tmp/a.sock".into()])
    );
    assert!(w.is_empty());
}

#[test]
fn test_relative_reported() {
    let mut c = settings();
    c.allow_unix_sockets = vec!["relative.sock".into()];
    let (p, w) = network_policy_from(&c);
    assert_eq!(
        p.unix_sockets,
        UnixSockets::Denied,
        "an unusable entry must not leave a wider posture behind"
    );
    assert_eq!(w.len(), 1, "the dropped entry must be reported");
    assert!(w[0].contains("relative.sock"));
}

#[test]
fn test_mixed_keeps_usable() {
    let mut c = settings();
    c.allow_unix_sockets = vec!["bad.sock".into(), "/tmp/good.sock".into()];
    let (p, w) = network_policy_from(&c);
    assert_eq!(
        p.unix_sockets,
        UnixSockets::Paths(vec!["/tmp/good.sock".into()])
    );
    assert_eq!(w.len(), 1);
}

#[test]
fn test_blanket_overrides_list() {
    // The settings can express a contradiction the enforced shape cannot. The
    // blanket flag wins, and the ignored list is reported, because a config that
    // reads as an allowlist while behaving as allow-everything is a posture the
    // user did not choose.
    let mut c = settings();
    c.allow_all_unix_sockets = true;
    c.allow_unix_sockets = vec!["/tmp/a.sock".into()];
    let (p, w) = network_policy_from(&c);
    assert_eq!(p.unix_sockets, UnixSockets::All);
    assert_eq!(w.len(), 1, "the ignored list must be reported");
    assert!(w[0].contains("allow_all_unix_sockets"));
}

#[test]
fn test_blanket_stays_quiet() {
    let mut c = settings();
    c.allow_all_unix_sockets = true;
    let (p, w) = network_policy_from(&c);
    assert_eq!(p.unix_sockets, UnixSockets::All);
    assert!(w.is_empty(), "no contradiction, so nothing to report");
}

#[test]
fn test_sockets_deny_egress() {
    // Pin an explicit off so the assertion isolates "unix sockets are
    // local IPC and must not widen egress" — under the default open
    // posture, egress is open by the default, not by sockets.
    let mut c = settings();
    c.mode = NetworkMode::Off;
    c.allow_all_unix_sockets = true;
    let (p, _) = network_policy_from(&c);
    assert!(
        !p.allows_egress(),
        "unix sockets are local IPC and must not widen egress"
    );
}

#[test]
fn test_unsupported_mode_denies() {
    // Fail closed, and report. A mode this version cannot enforce must not be
    // approximated by the nearest thing it can do if that thing is wider.
    let mut c = settings();
    c.mode = NetworkMode::Unsupported("proxy".into());
    let (p, w) = network_policy_from(&c);
    assert!(!p.allows_egress());
    assert!(w.iter().any(|m| m.contains("proxy")), "got {w:?}");
}

#[test]
fn test_deferred_keys_surface() {
    // The mapping is the one place the user hears about a setting that was read
    // but not enforced, so it must pass the config's own report through.
    let mut c = settings();
    c.unrecognized
        .insert("allow".into(), serde_json::json!(["corp.example"]));
    let (_, w) = network_policy_from(&c);
    assert!(w.iter().any(|m| m.contains("allow")), "got {w:?}");
}
