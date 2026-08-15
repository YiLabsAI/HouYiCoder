#!/usr/bin/env python3
"""In-function comment-block ratchet: the number of indented (in-function)
plain // comment blocks >= WARN_LINES must not increase above the baseline.

A "verbose comment" smell: a long run of consecutive // lines inside a
function explaining mechanics that the code already says, or retelling
control flow. The fix is to delete or condense, not to add. The gate
blocks NEW long blocks from landing; the baseline sinks as the existing
ones are pruned.

Shape (resolved): the first attempt gated ALL consecutive // blocks at
>=12 and hit 150 -- most were module-level file headers / section
separators (legit), so the false-positive rate made the gate noise. The
fix is to narrow to INDENTED blocks (leading whitespace = in-function),
and to exclude doc comments (/// and //!). After narrowing, >=12 drops
to 10 blocks (the real verbose-comment candidates).

Module-head //! / /// doc blocks and col-0 // section headers are exempt.
The threshold only targets in-function plain // explanation blocks.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.paths import is_test_file

WARN_LINES = 12  # a consecutive in-function // block this long is a
# verbose-comment candidate (warn band; >=20 would be the error band).
COMMENT_BLOCK_BASELINE = 10  # measured 2026-08-17, indented plain //


def ratchet_status(total, baseline=COMMENT_BLOCK_BASELINE) -> int:
    """Pure strict-pin: 0 only when total == baseline. Growth (>) and
    drift (<) both return 1. Pure so the regression test can lock both
    directions without scanning the repo tree."""
    return 0 if total == baseline else 1
# blocks >=12 across production crates (tests excluded). Was 150 before
# narrowing to in-function -- the narrowing removed the module-level
# false positives that had deferred this gate.


def _is_plain_inline_comment(line: str) -> bool:
    """True for a plain // explanation line (not a /// or //! doc line)."""
    s = line.strip()
    return s.startswith("//") and not s.startswith("///") and not s.startswith("//!")


def comment_blocks(root: Path) -> list[tuple[str, int, int]]:
    """Indented plain // blocks >= WARN_LINES, as (rel_path, start_line,
    length). A block is counted only if its first line is indented
    (leading whitespace) -- that is the in-function signal; col-0 blocks
    are module-level and exempt."""
    repo_root = Path(__file__).resolve().parents[1]
    out: list[tuple[str, int, int]] = []
    for f in sorted(root.rglob("*.rs")):
        # Path relative to the repo root for test-file exclusion + the
        # report key. Files outside the repo (e.g. test fixtures in
        # tmpdir) fall back to the resolved path -- they are not test
        # files by the repo's conventions, so the exclusion is a no-op
        # on them, and the key just identifies them by absolute path.
        try:
            rel = str(f.resolve().relative_to(repo_root))
        except ValueError:
            rel = str(f.resolve())
        if is_test_file(rel):
            continue
        try:
            lines = f.read_text(encoding="utf-8").splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        i = 0
        n = len(lines)
        while i < n:
            if lines[i][:1].isspace() and _is_plain_inline_comment(lines[i]):
                start = i
                while i < n and _is_plain_inline_comment(lines[i]):
                    i += 1
                length = i - start
                if length >= WARN_LINES:
                    out.append((rel, start + 1, length))
            else:
                i += 1
    return sorted(out, key=lambda x: -x[2])


def main() -> int:
    repo = Path(__file__).resolve().parents[1] / "crates"
    blocks = comment_blocks(repo)
    total = len(blocks)
    if total > COMMENT_BLOCK_BASELINE:
        print(
            f"error: comment-block ratchet breached: {total} blocks >= "
            f"{WARN_LINES} lines > {COMMENT_BLOCK_BASELINE} baseline "
            f"(+{total - COMMENT_BLOCK_BASELINE}). Prune or condense the "
            f"new verbose // block (state the why, not the mechanics); "
            f"module-head //! and col-0 section headers are exempt. If "
            f"this is a real in-function explanation, condense it and "
            f"keep the baseline tight.",
            file=sys.stderr,
        )
        for rel, start, length in blocks[:8]:
            print(f"  {length} lines: {rel}:{start}", file=sys.stderr)
        return 1
    if total < COMMENT_BLOCK_BASELINE:
        print(
            f"error: comment-block ratchet drifted: {total} < "
            f"{COMMENT_BLOCK_BASELINE} (-{COMMENT_BLOCK_BASELINE - total}). "
            f"Lower COMMENT_BLOCK_BASELINE to {total} in this commit so the "
            f"floor tracks reality (strict-pin: drift blocks until fixed).",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
