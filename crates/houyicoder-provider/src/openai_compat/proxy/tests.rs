//! Tests for the proxy client construction.

use super::*;

/// A client builds with the ring provider installed, so a process that
/// builds its first HTTP client through this path can speak TLS at all.
#[test]
fn test_client_builds_with_provider() {
    install_tls_provider();
    let snapshot = ProxySnapshot::direct();
    let client = build_client(&snapshot).expect("the client builds");
    drop(client);
}
