//! Which fence axis each relaxation rests on. A fence reports its coverage
//! and, separately, which of writes, reads, and the network it actually
//! confines. Every relaxation below is a single-axis premise, so a fence that
//! confines one axis must not earn another axis's relaxation: a read-only
//! mount sees reads without bounding where a write lands, and a fence that
//! bounds writes without hiding reads has not earned the read waiver.

use super::*;
use crate::decision::Outcome;
use houyicoder_api::sandbox::{Containment, Coverage, FenceCapability, SideEffect};
use std::path::PathBuf;
use std::sync::Arc;

/// A fence whose path rules cover the session but confine only reads.
struct ReadsOnlyFence;

impl Containment for ReadsOnlyFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![PathBuf::from("/ws")],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn capability(&self) -> FenceCapability {
        FenceCapability {
            writes: false,
            reads: true,
            network: false,
        }
    }
}

/// A fence whose path rules cover the session but confine only writes.
struct WritesOnlyFence;

impl Containment for WritesOnlyFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![PathBuf::from("/ws")],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn capability(&self) -> FenceCapability {
        FenceCapability {
            writes: true,
            reads: false,
            network: false,
        }
    }
}

/// A fence that reports fenced coverage and takes the default capability:
/// both path axes contained, the network axis open. This is the shape a
/// path-based backend has, and the positive control for the writes and read
/// relaxations below.
struct FilesystemFence;

impl Containment for FilesystemFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![PathBuf::from("/ws")],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
}

/// The same path coverage with every axis withdrawn: a backend that reports
/// a fence without enforcing an axis owes the gate nothing on it, so each
/// relaxation must stay a question.
struct WithdrawnFence;

impl Containment for WithdrawnFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![PathBuf::from("/ws")],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
    fn capability(&self) -> FenceCapability {
        FenceCapability::NONE
    }
}

fn fenced_mode(mode: PermissionMode, containment: Arc<dyn Containment>) -> DefaultModeGate {
    DefaultModeGate::with_mode(mode).with_containment(containment)
}

/// The writes relaxations -- a temporary removal and a bounded redirect --
/// each rest on the fence confining writes. With path coverage alone, without
/// the writes axis, the same commands ask again, so an allow here cannot come
/// from the file name or the suffix.
#[test]
fn test_writes_relaxations_need_axis() {
    let confined = fenced_mode(PermissionMode::Auto, Arc::new(FilesystemFence));
    let reads_only = fenced_mode(PermissionMode::Auto, Arc::new(ReadsOnlyFence));
    for command in [
        "rm -rf \"$TMPDIR/fresh\"",
        "make check > \"$TMPDIR/out.log\" 2>&1",
    ] {
        assert_eq!(
            confined.decide(&bash_req(command)).outcome(),
            Outcome::Allow,
            "a writes-confined fence bounds the effect: {command}"
        );
        assert_eq!(
            reads_only.decide(&bash_req(command)).outcome(),
            Outcome::Ask,
            "a reads-only fence does not bound the write: {command}"
        );
    }
}

/// A download whose destination is a scratch file is only safe to allow
/// without a question because the fence bounds where the download lands.
#[test]
fn test_scratch_download_asks() {
    let command = "curl -sS https://api.github.com/repos/a/b -o \"$TMPDIR/pr.json\"";
    let confined = DefaultModeGate::with_mode(PermissionMode::Auto)
        .with_trusted_hosts()
        .with_containment(Arc::new(FilesystemFence));
    let reads_only = DefaultModeGate::with_mode(PermissionMode::Auto)
        .with_trusted_hosts()
        .with_containment(Arc::new(ReadsOnlyFence));
    assert_eq!(
        confined.decide(&bash_req(command)).outcome(),
        Outcome::Allow
    );
    assert_eq!(
        reads_only.decide(&bash_req(command)).outcome(),
        Outcome::Ask
    );
}

/// The Manual read waiver rests on the fence's read visibility: the command
/// reads only what the profile permits, so nothing else is visible to read.
/// A fence that confines writes but not reads has not earned it.
#[test]
fn test_read_waiver_needs_reads() {
    let reads_confined = fenced_mode(PermissionMode::Manual, Arc::new(ReadsOnlyFence));
    let writes_only = fenced_mode(PermissionMode::Manual, Arc::new(WritesOnlyFence));
    for command in ["git status", "rg clippy crates/"] {
        assert_eq!(
            reads_confined.decide(&bash_req(command)).outcome(),
            Outcome::Allow,
            "a reads-confined fence covers the proof: {command}"
        );
        assert_eq!(
            writes_only.decide(&bash_req(command)).outcome(),
            Outcome::Ask,
            "a writes-only fence does not cover the read: {command}"
        );
    }
}

/// A backend that reports fenced coverage while withdrawing every axis keeps
/// every relaxation a question: the premise of each is an axis the fence
/// says it does not enforce, so nothing may be granted on its word.
#[test]
fn test_withdrawn_fence_asks() {
    let auto = fenced_mode(PermissionMode::Auto, Arc::new(WithdrawnFence));
    for command in [
        "rm -rf \"$TMPDIR/fresh\"",
        "make check > \"$TMPDIR/out.log\" 2>&1",
    ] {
        assert_eq!(
            auto.decide(&bash_req(command)).outcome(),
            Outcome::Ask,
            "no writes axis: {command}"
        );
    }
    let manual = fenced_mode(PermissionMode::Manual, Arc::new(WithdrawnFence));
    for command in ["git status", "rg clippy crates/"] {
        assert_eq!(
            manual.decide(&bash_req(command)).outcome(),
            Outcome::Ask,
            "no reads axis: {command}"
        );
    }
}
