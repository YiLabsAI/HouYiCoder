//! The liveness probe over the process this test runs in.

use super::alive;
use std::process::id;

/// The current process is alive and an identifier no process holds is not. The
/// sentinel sits above the range either system allocates, so it names nothing.
/// Zero and negative values name no individual process, so a corrupt record
/// holding one is never mistaken for a live session.
#[test]
fn test_alive_self_dead() {
    assert!(alive(id() as i32), "the current process is alive");
    assert!(!alive(1_000_000_000), "a non-existent process is not alive");
    assert!(!alive(0), "zero names no process");
    assert!(!alive(-1), "a negative identifier names no process");
}
