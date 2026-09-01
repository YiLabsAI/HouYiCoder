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
     python3 scripts/cov_lcov.py --lcov-path
  --check exits 0 if no stale mapping found, 2 if found (with evidence
  printed to stderr), 1 if the lcov file cannot be read. --cov-dir prints
  the resolved shared instrumented-cache dir (used by the gates + shell).
  --lcov-path prints this worktree's report path inside that dir.
"""
from __future__ import annotations

import hashlib
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def cov_target_dir() -> str:
    """The instrumented-build cache dir for THIS worktree, overridable with
    HOUYICODER_COV_DIR.

    It was shared across worktrees for a while, to spare a new worktree the
    cold instrumented build. That is not sound: a coverage report is derived
    from the objects in the target dir, so a sibling worktree's test binaries
    join the report and bring their own line tables, and the stale-mapping
    guard then refuses to draw a verdict -- deterministically, for every
    worktree whose sibling built more recently, not only under concurrent
    runs. Two other hazards rode along: the report was written under one
    shared name, and a rebuild-on-stale wipes the whole tree out from under
    whoever else is using it.

    The cold cost the sharing was paying for belongs to the dependency graph,
    which sccache caches by content -- so it is recovered without any shared
    mutable state (RUSTC_WRAPPER is wired in run_tests.py)."""
    override = os.environ.get("HOUYICODER_COV_DIR")
    if override:
        return override
    return os.path.join("target", "cov")


def lcov_path(cov_dir: str | None = None) -> str:
    """This worktree's report path inside the shared cache dir.

    Sharing the build cache is safe -- cargo keys artifacts by content. A
    report is not an artifact of that kind: it is a snapshot of one
    worktree's sources, so a single shared name lets whichever worktree ran
    last hand its line table to the next worktree's gate, which the
    stale-mapping check above then correctly refuses. One name per writer.
    """
    base = cov_dir if cov_dir is not None else cov_target_dir()
    root = _worktree_root()
    slug = hashlib.sha1(root.encode()).hexdigest()[:10]
    return os.path.join(base, f"houyi-cov-{os.path.basename(root)}-{slug}.lcov")


def _worktree_root() -> str:
    """This checkout's root, which identifies the worktree. Falls back to the
    script's own parent so a non-git invocation still yields a stable name."""
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, timeout=5,
        )
        if out.returncode == 0 and out.stdout.strip():
            return os.path.normpath(out.stdout.strip())
    except (OSError, subprocess.SubprocessError):
        pass
    return str(ROOT)


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


def check(report: Path) -> int:
    """Parse the lcov, detect stale mapping, print evidence, return exit code."""
    if not report.is_file():
        print(f"error: lcov report not found at {report}", file=sys.stderr)
        return 1
    executable = lcov_executable_lines(report)
    if not executable:
        print(f"error: no executable lines parsed from {report}", file=sys.stderr)
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
    if len(sys.argv) == 2 and sys.argv[1] == "--lcov-path":
        print(lcov_path())
        return 0
    if len(sys.argv) == 2 and sys.argv[1] == "--cov-dir":
        print(cov_target_dir())
        return 0
    if len(sys.argv) != 3 or sys.argv[1] != "--check":
        print(
            "usage: cov_lcov.py --check <lcov-path> | --cov-dir | --lcov-path",
            file=sys.stderr,
        )
        return 1
    return check(Path(sys.argv[2]))


if __name__ == "__main__":
    sys.exit(main())
