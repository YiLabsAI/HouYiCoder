#!/usr/bin/env python3
"""Check .rs file line-count limits, split by test vs production.

Production and test files carry different cognitive cost: a 1500-line
production file is 1500 lines of interacting logic (superlinear load);
a 1500-line test file is ~40 independent 35-line blocks (near-linear
load). One line-count threshold for both mixed two cost curves (test
and production metrics don't mix) and the 800 error gate parked 22
files in the 750-799 band -- a cliff whose pressure produced overflow
test files named by production history (_extra_tests) rather than by
behavior domain.

Production keeps the 500 warn / 800 error cliff (a per-file cap, stops
one file ballooning) AND gains a continuous excess ratchet (an aggregate
cap, stops proliferation). The excess metric sums max(0, lines - 700)
over production files; it is continuous: 699 -> 700 adds +0 (no new
cliff at 700), 700 -> 705 adds +5, a 798-line file split into 400 + 398
drops the sum by 98. Pressure falls only on files already past 700
lines; ~90% of production files are unaffected.

Test files relax to 800 warn / 2000 error. 2000 is a pathological
backstop (a single test file so large something is structurally wrong),
NOT a target -- the right test-file size is driven by cohesion (one
behavior domain), which is review judgment, not a countable proxy. The
old 800 gate polluted the test data (max test file 799, pressed down by
the gate), so no calibration from it would be honest. The real test-side
pressure is the 800 warn (large test files surface in make check) +
Q7 cohesion review on touch. There is NO test-side excess ratchet:
test cognitive load is near-linear, so a ratchet degenerates to a cliff
at 800 -- the asymmetry with production is the application of judgment 16
(test/production metrics don't mix), not a gap to close.
"""
import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.paths import is_test_file  # noqa: E402

PROD_WARN, PROD_ERR = 500, 800
TEST_WARN, TEST_ERR = 800, 2000
EXCESS_THRESHOLD = 700
EXCESS_BASELINE = 2135


def ratchet_status(excess, baseline=EXCESS_BASELINE) -> int:
    """Ceiling ratchet: 0 when excess <= baseline (pass), 1 when excess >
    baseline (block). Improvements (lower excess) pass silently; only growth
    blocks. The baseline is raised in the same commit that grows files; it
    never needs lowering when files shrink. Pure so the regression test can
    lock the ceiling behavior without scanning files."""
    return 0 if excess <= baseline else 1


def evaluate(path_lines):
    """Pure evaluator. path_lines: list of (rel_path_str, line_count).

    Returns a dict: warns, errs (per-file cliff hits), excess (the
    continuous sum), excess_contrib (top offenders). Pure so the
    regression test can lock the no-cliff / continuity properties without
    touching the filesystem.
    """
    warns, errs, excess_contrib = [], [], []
    excess = 0
    for rel, n in path_lines:
        test = is_test_file(rel)
        warn, err = (TEST_WARN, TEST_ERR) if test else (PROD_WARN, PROD_ERR)
        if n >= err:
            errs.append((rel, n, test))
        elif n >= warn:
            warns.append((rel, n, test))
        if not test and n > EXCESS_THRESHOLD:
            ex = n - EXCESS_THRESHOLD
            excess += ex
            excess_contrib.append((ex, n, rel))
    excess_contrib.sort(reverse=True)
    return {"warns": warns, "errs": errs, "excess": excess,
            "excess_contrib": excess_contrib}


def main():
    ap = argparse.ArgumentParser(
        description="Check .rs file line-count limits, split by test vs production")
    ap.add_argument("files", nargs="*", help=".rs files to inspect")
    a = ap.parse_args()

    files = [Path(p) for p in a.files if p.endswith(".rs")] or list(
        Path("crates").rglob("*.rs")
    )
    path_lines = []
    for p in files:
        if not p.is_file() or "/target/" in p.as_posix():
            continue
        try:
            n = len(p.read_text(encoding="utf-8").splitlines())
        except OSError:
            continue
        path_lines.append((p.as_posix(), n))

    r = evaluate(path_lines)

    for rel, n, test in r["warns"]:
        lim = TEST_WARN if test else PROD_WARN
        kind = "test" if test else "prod"
        print(f"[FileSize] WARN: {rel} ({n} lines, {kind}, >= {lim}) -- refactor soon")

    if r["errs"]:
        print(f"[FileSize] ERROR: {len(r['errs'])} file(s) over the per-file "
              f"limit -- refactor/split:")
        for rel, n, test in sorted(r["errs"], key=lambda x: -x[1]):
            lim = TEST_ERR if test else PROD_ERR
            kind = "test" if test else "prod"
            print(f"  - {rel} ({n} lines, {kind} >= {lim})")
        return 1

    if r["excess"] > EXCESS_BASELINE:
        print(f"[FileSize] ERROR: production excess (sum of max(0, lines - "
              f"{EXCESS_THRESHOLD})) = {r['excess']} over baseline "
              f"{EXCESS_BASELINE} (ceiling). Top contributors:")
        for ex, n, rel in r["excess_contrib"][:5]:
            print(f"  +{ex:4d}  ({n} lines)  {rel}")
        print("  Trim, delete, or split by behavior domain (not by overflow).")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
