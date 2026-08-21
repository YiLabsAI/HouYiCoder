#!/usr/bin/env python3
"""Check .rs file line-count limits, split by test vs production.

Production and test files sit on different cost curves, so they get
different limits. Production lines interact, so cost grows superlinearly
with length; test files are independent blocks, so cost grows about
linearly. One threshold for both averages two curves and fits neither.

Production gets two limits that catch different failures. The per-file
cliff stops a single file ballooning. The excess ratchet -- the sum of
max(0, lines - threshold) across production files, compared against a
ceiling -- stops many files each parking just under the cliff, which no
per-file rule can see. Summing an overage rather than counting offenders
keeps it continuous, so crossing the threshold costs nothing and a split
repays exactly what it removes; there is no second cliff to game.

Test files get the per-file limits only. Their error limit is a
pathological backstop, not a target: the right size for a test file is
one behavior domain, which is a review judgment and not a countable
proxy. No excess ratchet here -- on a linear cost curve the sum carries
no information the per-file limit lacks, so it would only recreate the
cliff it exists to soften.
"""
import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.paths import is_test_file  # noqa: E402

PROD_WARN, PROD_ERR = 500, 800
TEST_WARN, TEST_ERR = 800, 2000
EXCESS_THRESHOLD = 700
# Ceiling for production excess. Raise only for real API growth on files
# already past the floor, after absorption is exhausted, in the same commit
# that grows them; lower it back when a split absorbs it. Why a given bump
# happened belongs in that commit message -- recording it here builds a
# changelog in a file that never forgets, and the reasons go stale while the
# number moves on.
EXCESS_BASELINE = 2159


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
