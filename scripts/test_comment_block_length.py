#!/usr/bin/env python3
"""Regression tests for the comment-block gate's indented-only narrowing.

The gate's value lives entirely in the narrowing: a plain // block is
counted ONLY if indented (in-function). Col-0 blocks (module-level
headers / section separators), /// doc, and //! doc are all exempt.
Without this narrowing the gate hit 150 -- mostly module-level false
positives -- and was deferred as noise; with it, 10. A silent regression
in the indent/doc test would keep baseline=10 green while the count
drifts back toward 150: judgment 11 (wrong population, gate still
green). This test locks the narrowing.

PAIRED: one fixture mixes the counted shape (indented plain //) with
three exempt shapes of EQUAL length (col-0 //, //!, ///) plus a
sub-threshold indented block. Asserting only the indented plain //
block is returned forces the gate to distinguish indent + doc kind,
not just length.
"""
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_comment_block_length as m  # noqa: E402

# Shrink the threshold so fixtures stay compact (3 lines, not 12).
# The narrowing logic is threshold-independent; only the boundary
# test below depends on the value.
m.WARN_LINES = 3

FIXTURE = """\
//! module doc line one
//! module doc line two
//! module doc line three
// col-zero section header one
// col-zero section header two
// col-zero section header three
fn foo() {
    // indented in-function one
    // indented in-function two
    // indented in-function three
    let x = 1;
    /// doc on an item one
    /// doc on an item two
    /// doc on an item three
    let y = 2;
    // short indented one
    // short indented two
}
"""


def main() -> int:
    # strict-pin: both growth and drift block (locks the strict behavior
    # so a future relaxation to advisory is caught here).
    if m.ratchet_status(m.COMMENT_BLOCK_BASELINE + 1) != 1:
        print("FAIL: growth must block", file=sys.stderr)
        return 1
    if m.ratchet_status(m.COMMENT_BLOCK_BASELINE - 1) != 1:
        print("FAIL: drift must block (strict-pin)", file=sys.stderr)
        return 1
    if m.ratchet_status(m.COMMENT_BLOCK_BASELINE) != 0:
        print("FAIL: exact must be green", file=sys.stderr)
        return 1
    root = Path(tempfile.mkdtemp())
    (root / "fixture.rs").write_text(FIXTURE)
    blocks = m.comment_blocks(root)
    failures: list[str] = []

    # Exactly one block: the indented plain // (3 lines). The //! module
    # doc, the col-0 // section header, the /// item doc, and the
    # sub-threshold indented pair must all be excluded.
    if len(blocks) != 1:
        failures.append(
            f"expected 1 counted block, got {len(blocks)}: {blocks}"
        )
    else:
        _rel, start, length = blocks[0]
        if length != 3:
            failures.append(f"expected the 3-line indented block, got length {length}: {blocks}")
        if "fixture.rs" not in _rel:
            failures.append(f"expected fixture.rs in the key, got {_rel}")

    # Boundary: at WARN_LINES=3, a 3-line block counts and a 2-line does
    # not. The fixture already carries both (the indented 3 counts, the
    # indented 2 does not); the len==1 assertion above covers both. This
    # extra check makes the boundary intent explicit rather than implied.
    short_root = Path(tempfile.mkdtemp())
    (short_root / "s.rs").write_text("fn f() {\n    // a\n    // b\n}\n")
    if m.comment_blocks(short_root):
        failures.append(
            f"sub-threshold (2-line) block must not count: {m.comment_blocks(short_root)}"
        )

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        return 1
    print("comment-block narrowing: indented-only + doc/col-0 exempt + boundary ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
