//! Tests for the network segment builders.
//!
//! These assert on emitted profile text, which is the only place the two kernel
//! traps documented on the builders can be pinned. The rule that outbound must
//! filter on the remote address gets a dedicated negative test, because the
//! wrong spelling produces a profile that looks contained and is not, and no
//! text-level test elsewhere would notice. Whether the kernel then honours the
//! text is a separate question that the kernel probe answers.

use super::*;
use houyicoder_api::sandbox::Egress;

fn contained() -> NetworkPolicy {
    NetworkPolicy::contained()
}

fn open() -> NetworkPolicy {
    let mut p = NetworkPolicy::contained();
    p.egress = Egress::Unrestricted;
    p
}

#[test]
fn test_contained_denies_class() {
    let s = network_rules(&contained(), "tag-n");
    assert_eq!(s, "(deny network* (with message \"tag-n\"))\n");
}

#[test]
fn test_open_allows_class() {
    let s = network_rules(&open(), "tag-n");
    assert_eq!(s, "(allow network*)\n");
    assert!(
        !s.contains("deny"),
        "the open posture must not also emit a deny that shadows it"
    );
}

#[test]
fn test_open_ignores_allowbacks() {
    // With the whole class allowed, the local allow-backs are redundant. Emitting
    // them anyway would be dead profile text that invites the reader to think the
    // posture is narrower than it is.
    let mut p = open();
    p.allow_local_binding = true;
    p.unix_sockets = UnixSockets::All;
    assert_eq!(network_rules(&p, "tag-n"), "(allow network*)\n");
}

#[test]
fn test_binding_follows_deny() {
    // Last-match-wins: the allow-back is inert unless it lands after the deny.
    let mut p = contained();
    p.allow_local_binding = true;
    let s = network_rules(&p, "tag-n");
    let deny = s.find("(deny network*").expect("class deny present");
    let allow = s.find("(allow network-bind").expect("bind allow present");
    assert!(
        deny < allow,
        "allow-back must follow the deny it carves out"
    );
}

#[test]
fn test_binding_wildcards_host() {
    // The loopback host token does not match the v4-mapped v6 address a
    // dual-stack socket binds to, so bind and inbound must use the wildcard.
    let mut p = contained();
    p.allow_local_binding = true;
    let s = network_rules(&p, "tag-n");
    assert!(s.contains("(allow network-bind (local ip \"*:*\"))"));
    assert!(s.contains("(allow network-inbound (local ip \"*:*\"))"));
}

#[test]
fn test_outbound_filters_remote() {
    // The trap: a local-address filter on outbound is matched against the source
    // address, which is the any-address before bind, and the loopback token
    // matches the any-address. Spelled that way this rule would allow egress to
    // every host. Assert the remote-address spelling, and assert the local-address
    // spelling is absent so a future edit cannot reintroduce it quietly.
    let mut p = contained();
    p.allow_local_binding = true;
    let s = network_rules(&p, "tag-n");
    assert!(s.contains("(allow network-outbound (remote ip \"localhost:*\"))"));
    assert!(
        !s.contains("(allow network-outbound (local ip"),
        "outbound filtered on the local address admits every host"
    );
}

#[test]
fn test_binding_denies_egress() {
    let mut p = contained();
    p.allow_local_binding = true;
    assert!(!p.allows_egress());
    let s = network_rules(&p, "tag-n");
    assert!(!s.contains("(allow network*)"));
}

#[test]
fn test_denied_emits_nothing() {
    let s = network_rules(&contained(), "tag-n");
    assert!(!s.contains("system-socket"));
    assert!(!s.contains("unix-socket"));
}

#[test]
fn test_paths_allow_operations() {
    // Creating the socket carries no path so it needs its own rule; bind matches
    // the local path and connect the remote one. Missing any of the three breaks
    // the whole path.
    let mut p = contained();
    p.unix_sockets = UnixSockets::Paths(vec!["/tmp/agent.sock".into()]);
    let s = network_rules(&p, "tag-n");
    assert!(s.contains("(allow system-socket (socket-domain AF_UNIX))"));
    assert!(s.contains("(allow network-bind (local unix-socket (subpath \"/tmp/agent.sock\")))"));
    assert!(
        s.contains("(allow network-outbound (remote unix-socket (subpath \"/tmp/agent.sock\")))")
    );
}

#[test]
fn test_creation_emitted_once() {
    let mut p = contained();
    p.unix_sockets = UnixSockets::Paths(vec!["/tmp/a.sock".into(), "/tmp/b.sock".into()]);
    let s = network_rules(&p, "tag-n");
    assert_eq!(s.matches("(allow system-socket").count(), 1);
    assert!(s.contains("\"/tmp/a.sock\""));
    assert!(s.contains("\"/tmp/b.sock\""));
}

#[test]
fn test_relative_path_dropped() {
    // A relative path can never match, so emitting it would produce a rule that
    // reads as effective and is inert.
    let mut p = contained();
    p.unix_sockets = UnixSockets::Paths(vec!["relative/agent.sock".into()]);
    let s = network_rules(&p, "tag-n");
    assert!(!s.contains("unix-socket"));
    assert!(
        !s.contains("system-socket"),
        "no usable path means no socket creation either"
    );
}

#[test]
fn test_paths_keep_absolute() {
    let mut p = contained();
    p.unix_sockets = UnixSockets::Paths(vec!["bad.sock".into(), "/tmp/good.sock".into()]);
    let s = network_rules(&p, "tag-n");
    assert!(s.contains("\"/tmp/good.sock\""));
    assert!(!s.contains("bad.sock"));
}

#[test]
fn test_quote_escaped() {
    // A quote in a config-supplied path must not close the literal early; raw
    // interpolation here would let the rest of the path be parsed as profile
    // syntax and inject rules.
    let mut p = contained();
    p.unix_sockets = UnixSockets::Paths(vec!["/tmp/a\") (allow network*) (allow x".into()]);
    let s = network_rules(&p, "tag-n");
    // The payload does still appear in the text, inside the quoted literal where
    // it is inert, so the assertion is line-based: one rule per line means an
    // injected rule would have to start a line.
    assert!(
        !s.lines()
            .any(|l| l.trim_start().starts_with("(allow network*")),
        "an embedded quote must not inject a rule: {s}"
    );
    assert!(s.contains("\\\""), "the quote should be escaped in place");
}

#[test]
fn test_backslash_escaped() {
    // A trailing backslash would otherwise escape the closing quote and run the
    // literal into the following text.
    let mut p = contained();
    p.unix_sockets = UnixSockets::Paths(vec!["/tmp/dir\\".into()]);
    let s = network_rules(&p, "tag-n");
    assert!(s.contains("(subpath \"/tmp/dir\\\\\")"), "got {s}");
}

#[test]
fn test_blanket_allows_any() {
    let mut p = contained();
    p.unix_sockets = UnixSockets::All;
    let s = network_rules(&p, "tag-n");
    assert!(s.contains("(allow system-socket (socket-domain AF_UNIX))"));
    assert!(s.contains("(allow network-bind (local unix-socket (path-regex #\"^/\")))"));
    assert!(s.contains("(allow network-outbound (remote unix-socket (path-regex #\"^/\")))"));
    assert!(
        !s.contains("(allow network*)"),
        "unix sockets are local IPC and must not widen egress"
    );
}
