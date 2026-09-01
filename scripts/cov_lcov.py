#!/usr/bin/env python3
"""Shared lcov parser + stale-mapping detector for the two coverage gates.

Both check_diff_coverage.py (diff-cov) and check_coverage.sh (workspace
total) consume an lcov report. A stale report -- one whose baked-in
line table predates the last source edit -- attributes every number to the
wrong code, and the error runs both ways: it invents uncovered lines where
nothing executable now sits, and it credits new code with hits that belong
to whatever previously occupied those line numbers. A report that fails
open is the more dangerous half, because a passing gate is not audited.

This module holds the parse + detect + reject so both gates use one truth.
The check is deliberately weak but sound: only lines past the end of the
source file count as evidence. The tempting stronger check -- flagging a
line the report calls executable that is blank or comment-only -- does not
hold, because a region spanning several lines is projected onto every line
it covers, so comments inside a multi-line expression legitimately carry an
entry. That check was tried and fired on a correct report.

CLI: python3 scripts/cov_lcov.py --check <lcov-path>
     python3 scripts/cov_lcov.py --cov-dir
  --check exits 0 if no stale mapping found, 2 if found (with evidence
  printed to stderr), 1 if the lcov file cannot be read. --cov-dir prints
  the resolved shared instrumented-cache dir (used by the gates + shell).
"""
from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def cov_target_dir() -> str:
    """Resolve the shared instrumented-build cache dir so every worktree
    reuses the main checkout's target/cov instead of a cold per-worktree
    rebuild. git-common-dir is the main checkout's .git path; its parent is
    the main checkout root, where target/cov actually lives (not inside
    .git). Falls back to a local target/cov when that cannot be resolved."""
    fallback = os.path.join("target", "cov")
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--git-common-dir"],
            capture_output=True, text=True, timeout=5,
        )
        if out.returncode != 0:
            return fallback
        common = os.path.normpath(out.stdout.strip())
        if not os.path.isabs(common):
            common = os.path.normpath(os.path.join(str(ROOT), common))
        shared = os.path.join(os.path.dirname(common), "target", "cov")
        if os.path.isdir(os.path.dirname(shared)):
            return shared
        return fallback
    except (OSError, subprocess.SubprocessError):
        return fallback


def normalize(path: str) -> str:
    """Map any path form (absolute, b/crates/..., crates/...) to crates/.../foo.rs."""
    i = path.find("crates/")
    return path[i:] if i >= 0 else path


def lcov_executable_lines(lcov_path: Path) -> dict[str, dict[int, bool]]:
    """Return {normalized_file: {line: covered_bool}} for every executable
    (DA-traced) line. Lines not in DA (attributes, comments, mod decls, blank)
    are absent -- they are not coverable and excluded from the denominator."""
    out: dict[str, dict[int, bool]] = {}
    cur_file = None
    try:
        text = lcov_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return out
    for line in text.splitlines():
        if line.startswith("SF:"):
            cur_file = normalize(line[3:])
            out.setdefault(cur_file, {})
        elif line.startswith("DA:") and cur_file is not None:
            parts = line[3:].split(",")
            try:
                ln = int(parts[0])
                hit = int(parts[1])
            except (ValueError, IndexError):
                continue
            out[cur_file][ln] = hit > 0
    return out


def stale_mapping_evidence(executable: dict, root: Path = ROOT) -> list[str]:
    """Lines the report places outside the file they belong to.

    See the module docstring for why only past-end-of-file positions count
    and why the stronger comment-line check was rejected.
    """
    evidence: list[str] = []
    for path, lines in executable.items():
        src = root / path
        try:
            total = len(src.read_text(encoding="utf-8").splitlines())
        except OSError:
            continue
        for ln in sorted(lines):
            if ln > total:
                evidence.append(f"{path}:{ln} (file has {total} lines)")
    return evidence


def check(lcov_path: Path) -> int:
    """Parse the lcov, detect stale mapping, print evidence, return exit code."""
    if not lcov_path.is_file():
        print(f"error: lcov report not found at {lcov_path}", file=sys.stderr)
        return 1
    executable = lcov_executable_lines(lcov_path)
    if not executable:
        print(f"error: no executable lines parsed from {lcov_path}", file=sys.stderr)
        return 1
    evidence = stale_mapping_evidence(executable)
    if evidence:
        print(
            "error: the coverage report describes source that is not the source on "
            "disk, so no verdict can be drawn from it.",
            file=sys.stderr,
        )
        for e in evidence[:20]:
            print(f"  {e}", file=sys.stderr)
        return 2
    return 0


def main() -> int:
    if len(sys.argv) == 2 and sys.argv[1] == "--cov-dir":
        print(cov_target_dir())
        return 0
    if len(sys.argv) != 3 or sys.argv[1] != "--check":
        print("usage: cov_lcov.py --check <lcov-path> | --cov-dir", file=sys.stderr)
        return 1
    return check(Path(sys.argv[2]))


if __name__ == "__main__":
    sys.exit(main())
