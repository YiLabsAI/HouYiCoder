//! Kernel probes for the macOS Seatbelt network posture: egress containment,
//! loopback opt-in, and unix domain socket allow-backs.
//!
//! Every probe uses a literal address and never a hostname, so none of them
//! depends on DNS or on the host having working internet. The verbose flag on
//! the network tool prints the kernel's own not-permitted text on stderr,
//! distinguishing a fence refusal from a network timeout without relying on
//! latency thresholds.

use super::Fence;
use houyicoder_api::sandbox::{Egress, NetworkPolicy, UnixSockets};
use std::env::temp_dir;
use std::fs::{canonicalize, create_dir_all, remove_dir_all};
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::net::UnixListener;
use std::process::id;

/// The egress target: an address from the documentation-only range reserved by
/// RFC 5737 that cannot be routed anywhere.
const UNROUTABLE: &str = "192.0.2.1";

/// Bind a loopback listener in the test process and hand back its port, so a
/// probe can attempt a real loopback connection with a real peer on the other
/// end. Kept alive by the returned listener; dropping it closes the port.
fn loopback_listener() -> (TcpListener, u16) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let port = l.local_addr().expect("listener addr").port();
    (l, port)
}

/// The contained posture must actually stop egress at the kernel. This is the
/// baseline claim the whole containment story rests on, so it is asserted
/// against the kernel rather than against the profile text.
#[test]
fn test_contained_blocks_egress() {
    let f = Fence::with_network("net-off", NetworkPolicy::contained());
    let o = f.probe("egress", &format!("nc -v -G 1 -w 1 -z {UNROUTABLE} 80"));
    assert!(!o.allowed(), "the contained posture must refuse egress");
    assert!(
        o.denied_by_fence(),
        "the refusal must come from the fence rather than from the network: the \
         kernel names its own verdict on stderr, and a permitted connect to the \
         unreachable target reports a timeout instead. {}",
        o.detail()
    );
}

/// The open posture must actually reach the kernel too. Without this the
/// previous probe could pass for the wrong reason — a profile that denied
/// everything regardless of policy would satisfy it.
#[test]
fn test_open_permits_egress() {
    let mut p = NetworkPolicy::contained();
    p.egress = Egress::Unrestricted;
    let f = Fence::with_network("net-open", p);
    let o = f.probe("egress", &format!("nc -v -G 1 -w 1 -z {UNROUTABLE} 80"));
    assert!(
        !o.denied_by_fence(),
        "the open posture must let the connect through, which the unroutable \
         target then leaves to time out; a not-permitted verdict means the fence \
         blocked it. {}",
        o.detail()
    );
}

/// Loopback must be unreachable by default. A dev server left reachable under
/// the default posture would be an opening nobody asked for.
#[test]
fn test_contained_blocks_loopback() {
    let (_l, port) = loopback_listener();
    let f = Fence::with_network("net-lo-off", NetworkPolicy::contained());
    let o = f.probe("loopback", &format!("nc -v -G 1 -w 1 -z 127.0.0.1 {port}"));
    assert!(!o.allowed(), "loopback is not reachable by default");
    assert!(
        o.denied_by_fence(),
        "expected a fence denial, not a timeout. {}",
        o.detail()
    );
}

/// The loopback opt-in must reach loopback while still refusing egress.
///
/// The outbound rule that makes loopback work has to filter on the remote
/// address; spelled with a local-address filter instead it admits every host,
/// because the source address of an unbound socket is the any-address at
/// connect time and the loopback token matches it. Both halves are asserted
/// together so the outbound rule cannot fire wider than intended.
#[test]
fn test_loopback_grants_no_egress() {
    let (_l, port) = loopback_listener();
    let mut p = NetworkPolicy::contained();
    p.allow_local_binding = true;
    let f = Fence::with_network("net-lo-on", p);
    let lo = f.probe("loopback", &format!("nc -v -G 1 -w 1 -z 127.0.0.1 {port}"));
    assert!(
        lo.allowed(),
        "the loopback opt-in must actually reach a loopback peer. stderr: {}",
        lo.stderr
    );
    let out = f.probe("egress", &format!("nc -v -G 1 -w 1 -z {UNROUTABLE} 80"));
    assert!(
        out.denied_by_fence(),
        "the loopback opt-in must not widen egress; a timeout here instead of a \
         fence denial means the outbound rule matched every host, which is the \
         source-address trap. {}",
        out.detail()
    );
}

/// Unix sockets must be unreachable by default, and reachable when listed.
#[test]
fn test_socket_allowback_works() {
    let dir = temp_dir().join(format!("houyi-sock-{}", id()));
    create_dir_all(&dir).expect("create socket dir");
    let dir = canonicalize(&dir).expect("canonicalize socket dir");
    let path = dir.join("probe.sock");
    let listener = UnixListener::bind(&path).expect("bind unix socket");
    let cmd = format!("nc -v -w 1 -U {} </dev/null", path.display());

    let denied = Fence::with_network("sock-off", NetworkPolicy::contained());
    let off = denied.probe("socket", &cmd);
    assert!(
        !off.allowed(),
        "unix sockets are denied by default; the network class deny covers them"
    );

    let mut p = NetworkPolicy::contained();
    p.unix_sockets = UnixSockets::Paths(vec![dir.to_string_lossy().into_owned()]);
    let allowed = Fence::with_network("sock-on", p);
    let on = allowed.probe("socket", &cmd);
    assert!(
        on.allowed(),
        "a listed unix socket path must be reachable; a denial means one of the \
         three operations the allow-back emits is missing. stderr: {}",
        on.stderr
    );

    drop(listener);
    remove_dir_all(&dir).ok();
}
