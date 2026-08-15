"""Shared path classification — single source for "is this a test file"
+ "which .rs to scan" across the structure-facts detectors, so the
exclusion + scan range cannot drift between detectors.

Two questions, one source:
  - rs_source_files(repo_root): which .rs to scan (crates/, excl target/)
  - is_test_file(rel_path): of those, which are tests

is_test_file is the Python-side predicate; TEST_IGNORE_GLOBS is the
equivalent jscpd --ignore string (jscpd takes globs, not functions).
Both must stay in sync — a new test-naming convention that lands must
be added to both or the clone count silently includes test clones.

rs_source_files exists because a detector that scans the whole repo root
(REPO.rglob) hits worktree copies under the worktree tree — a 24x
inflation bug hit suppression_inventory this way. Centralizing the scan
range makes that error structurally impossible: a new detector calls
rs_source_files, never REPO.rglob.
"""
from __future__ import annotations

from pathlib import Path

_TEST_BASENAMES = {"tests.rs", "test_support.rs"}


def rs_source_files(repo_root):
    """Yield all .rs under crates/ (excluding target/). The single source
    for "which .rs to scan" so a detector cannot accidentally scan the
    repo root (which includes worktree copies). Pair with is_test_file to
    filter production vs test."""
    root = Path(repo_root) / "crates"
    for f in sorted(root.rglob("*.rs")):
        if "/target/" in str(f):
            continue
        yield f


def is_test_file(rel_path: str) -> bool:
    """True if rel_path is a test file under any of the repo's conventions."""
    p = rel_path.replace("\\", "/")
    name = p.rsplit("/", 1)[-1]
    if "/tests/" in ("/" + p + "/"):
        return True
    if name in _TEST_BASENAMES:
        return True
    if name.endswith("_tests.rs") or name.endswith("_test.rs"):
        return True
    return False


# jscpd --ignore globs — the glob form of is_test_file. Keep in sync with
# the function above; a new convention goes in both places.
TEST_IGNORE_GLOBS = ",".join([
    "**/tests/**",
    "**/tests.rs",
    "**/*_tests.rs",
    "**/*_test.rs",
    "**/test_support.rs",
])
