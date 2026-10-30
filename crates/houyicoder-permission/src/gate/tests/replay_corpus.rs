//! Replay of the commands that used to fire a confirmation card on every call
//! during a self-hosted run. Each one is a shape the gate now recognizes as safe under an
//! Auto session confined by a fence: a workspace-relative redirect, a copy into
//! a session-temporary child, a read-only sed of a protected skill file, a
//! double-quoted argument with an escaped quote, a local variable set from a
//! command substitution, and the safe spelling of a branch delete. The corpus
//! pins the fixes together; a regression on any one reddens the case that
//! exposed it.

use super::*;
use crate::decision::Outcome;
use houyicoder_api::sandbox::{Containment, Coverage, SideEffect};
use std::path::PathBuf;
use std::sync::Arc;

struct WorkspaceFence;
impl Containment for WorkspaceFence {
    fn coverage(&self) -> Coverage {
        Coverage::Fenced {
            writable_roots: vec![PathBuf::from("/ws")],
        }
    }
    fn would_block(&self, _effect: SideEffect) -> Option<String> {
        None
    }
}

fn gated() -> DefaultModeGate {
    DefaultModeGate::with_mode(crate::PermissionMode::Auto)
        .with_containment(Arc::new(WorkspaceFence))
        .with_auto_allow_fenced_exec(true)
}

#[test]
fn test_replay_corpus_allows() {
    let gate = gated();
    for command in [
        // A heredoc append into a workspace source file: the redirect target is
        // a safe relative path, and the heredoc body is stripped before the
        // scan.
        "cat >> crates/foo/tests.rs <<'RSEOF'\nassert!(true);\nRSEOF",
        // A copy whose destination is a session-temporary child.
        "cp crates/foo.rs /tmp/foo.bak",
        // A read-only sed paging a protected skill file.
        "sed -n '1,120p' /Users/von/.claude/skills/deep-review/SKILL.md",
        // A double-quoted argument with an escaped quote, which the scan once
        // misread as a closing quote.
        "grep -rn \"restore\\\"\" crates/",
        // A local variable set from a command substitution, which the env-name
        // check once flagged as a tmpdir setter.
        "S=$(ls -t /tmp | head -1); echo \"$S\"",
        // The safe spelling of a branch delete, which a heredoc body once
        // shadowed into a checkpoint ask.
        "git branch -d worktree-x",
    ] {
        let d = gate.decide(&bash_req(command));
        assert_eq!(d.outcome(), Outcome::Allow, "{command} => {d:?}");
    }
}

/// A copy with a flag still resolves its destination as the last operand, and
/// a sed that writes in place or carries a write command is not read-only, so
/// a protected skill file it touches asks rather than auto-allows.
#[test]
fn test_replay_writes_ask() {
    let gate = gated();
    assert_eq!(
        gate.decide(&bash_req("cp -r crates/foo /tmp/bar"))
            .outcome(),
        Outcome::Allow,
        "a copy with a flag still lands in the temp destination"
    );
    assert_eq!(
        gate.decide(&bash_req(
            "sed -i 's/x/y/' /Users/von/.claude/skills/deep-review/SKILL.md"
        ))
        .outcome(),
        Outcome::Ask,
        "an in-place sed is not read-only"
    );
    assert_eq!(
        gate.decide(&bash_req(
            "sed '/pat/w /tmp/out' /Users/von/.claude/skills/deep-review/SKILL.md"
        ))
        .outcome(),
        Outcome::Ask,
        "a sed write command is not read-only"
    );
}
