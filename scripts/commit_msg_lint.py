#!/usr/bin/env python3
"""Commit-msg lint. Reuses the SAME rules the PreToolUse hook applies to
.rs comments (rules/comments.py, shared with hook_rust.py), so write-time
and commit-time stay consistent and complementary -- one word source, two
enforcement points. Memory fades across sessions; this gate is the machine
backstop so the rules do not depend on the writer remembering them.

Reused from rules.comments (no reinvention):
  - CODENAME: internal task/sprint/gate tags (D1, F1, S26, T4, SEC-3,
    stage2, section refs, Bet A, B1-1, A1-B2 ...). Applied to subject + body.

Complementary (commit-specific, not in the .rs gate):
  - P0-P3 priority/severity tiers (in _ACCEPTANCE_CODES with the G/H/U/v
    codes): internal-doc indices a git-log reader cannot decode. Design docs
    (.md) are never scanned, so they keep carrying P0/P1/P2 legitimately.
  - ASCII-only subject: commit messages are English imperative (AGENTS.md).
    CJK in the subject trips here; the .rs gate trips CJK in comments.

NOT applied here (intentional complement, not an oversight):
  - OWN_NAME (houyicoder/hicoder, in rules.comments but not imported here):
    the .rs gate bans it because comments describe concerns, not crates. A
    commit subject legitimately names the crate it modifies
    (feat(ports): scaffold houyicoder-api), so banning own-name here would
    be wrong.

Reads the commit message from a file path arg (git commit-msg hook passes
the temp file). Exit 0 = ok, exit 1 = block (stderr shown to the author).
"""
import re
import sys
from pathlib import Path

# Reuse the shared comment-rule patterns (rules/comments.py), not a parallel
# wordlist. write-time hook (hook_rust.py) + check-time gate + commit-time all
# share one source.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.comments import CODENAME  # noqa: E402

# Acceptance/charter internal tracking codes the CODENAME regex misses: the
# phase-gate ids (G0-G5), the journey ids (U1-U8), the hazard ids (H1-H11),
# the priority/severity tiers (P0-P3), and the charter version stamps (v2.4).
# These are internal-doc indices an outside git-log reader cannot decode, so
# they are banned from commit prose. Commit-specific (kept out of hook_rust
# so .rs comments can still name a literal H1 heading or a v1.0 version
# without a false positive). P is commit-side only by the same reasoning and
# because design docs (.md, never scanned) legitimately carry P0/P1/P2
# priority labels.
_ACCEPTANCE_CODES = re.compile(r"\b[GHUP]\d+\b|\bv\d+\.\d+\b")

# Phase/sprint/gate/milestone/stage codes written as full words + space +
# number ("Phase 2", "Sprint 3", "Gate 1", "Milestone 5", "Stage 2"). These
# slip past _ACCEPTANCE_CODES (which expects letter+digit adjacent like P2,
# not "Phase 2" with a space) and past CODENAME (which catches "stage2"
# without space but not "Stage 2"). An outside git-log reader cannot decode
# "Phase 2" any more than "P2" -- ban both. re.IGNORECASE so "phase 2"
# lowercase is caught too. "Step N" is NOT included (too common in real
# commit instructions like "step 1: install").
_PHASE_WORDS = re.compile(
    r"\b(?:Phase|Sprint|Gate|Milestone|Stage)\s+\d+\b", re.IGNORECASE
)


def main() -> int:
    if len(sys.argv) < 2:
        print("commit_msg_lint: usage: commit_msg_lint.py <msg_file>", file=sys.stderr)
        return 1
    msg_path = Path(sys.argv[1])
    msg = msg_path.read_text(encoding="utf-8")
    lines = msg.splitlines()
    if not lines:
        return 0
    subject = lines[0]

    # 1. ASCII-only subject (English imperative, per AGENTS.md).
    non_ascii = [c for c in subject if ord(c) > 0x7F]
    if non_ascii:
        sample = "".join(non_ascii[:8])
        print(
            f"ERROR: commit subject must be ASCII (English, imperative). "
            f"Found non-ASCII {sample!r} in: {subject}",
            file=sys.stderr,
        )
        return 1

    # 2. Internal codenames: task/sprint tags + acceptance/charter codes.
    for i, line in enumerate(lines):
        m = (
            CODENAME.search(line)
            or _ACCEPTANCE_CODES.search(line)
            or _PHASE_WORDS.search(line)
        )
        if m:
            where = "subject" if i == 0 else f"body line {i + 1}"
            print(
                f"ERROR: commit message contains internal codename "
                f"'{m.group(0)}' ({where}). Write the concrete change, not "
                f"the task/sprint/gate/journey/version ID.",
                file=sys.stderr,
            )
            return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
