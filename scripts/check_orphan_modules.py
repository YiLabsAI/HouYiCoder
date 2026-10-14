#!/usr/bin/env python3
"""Orphan module check: every .rs under src/ must be reachable from a root.

A file that no `mod` declares is never compiled, so its contents are dead
weight that looks alive: a test file beside its owner reads as the test
suite, and an undeclared module reads as part of its owner. This happened when a test
module was extracted while the owner kept its inline copy -- the extracted
file was not declared, so the inline copy was the only one that ran, and
every gate stayed green.

The module tree is resolved from each crate root (lib.rs or main.rs)
following `mod name;` and `#[path = "..."]`, so a file reached only through
a nested module counts as reachable. A src/bin/*.rs is its own root and is
exempt: cargo compiles each one on its own, declared or not.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

# `mod name;` (no body) and `#[path = "file.rs"]` above a `mod`.
_MOD_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;")
_PATH_RE = re.compile(r'^\s*#\[path\s*=\s*"([^"]+)"\]')
# `mod name { .. }` is inline: it declares no file.
_INLINE_MOD_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z0-9_]*\s*\{")


def _declared(path: Path) -> list[tuple[Path, bool]]:
    """The files one source file declares, as (relative path, from #[path]).

    A #[path] and a mod may be separated by other attributes (cfg, allow),
    so an attribute line keeps the pending path rather than clearing it. A
    #[path] is relative to the declaring file's own directory; a bare mod
    name is relative to the directory named after the declaring file.
    """
    out, pending = [], None
    for line in path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("//"):
            continue
        m = _PATH_RE.match(line)
        if m:
            pending = m.group(1)
            continue
        if stripped.startswith("#["):
            continue
        m = _MOD_RE.match(line)
        if m:
            if pending:
                out.append((Path(pending), True))
            else:
                out.append((Path(f"{m.group(1)}.rs"), False))
        pending = None
    return out


def reachable(root: Path) -> set[Path]:
    """Every .rs the crate root can reach, resolving nested modules.

    A `mod name;` in X.rs resolves to X/name.rs or X/name/mod.rs -- rustc
    looks inside a directory named after the declaring file. A crate root
    (lib.rs, main.rs) has no such directory: its children sit beside it.
    """
    seen: set[Path] = set()
    stack = [root]
    while stack:
        current = stack.pop()
        if current in seen or not current.is_file():
            continue
        seen.add(current)
        stem_dir = current.parent
        if current.stem not in ("lib", "main", "mod"):
            stem_dir = current.parent / current.stem
        for rel, from_path in _declared(current):
            base = current.parent if from_path else stem_dir
            # `mod name;` is name.rs or name/mod.rs; a #[path] names the file.
            candidates = [base / rel]
            if rel.suffix == ".rs":
                candidates.append(base / rel.with_suffix("") / "mod.rs")
            for candidate in candidates:
                if candidate.is_file() and candidate not in seen:
                    stack.append(candidate)
    return seen


def orphans(crate_src: Path) -> list[Path]:
    """The .rs files under src/ that no root reaches."""
    roots = [crate_src / "lib.rs", crate_src / "main.rs"]
    bins = crate_src / "bin"
    if bins.is_dir():
        roots.extend(sorted(bins.glob("*.rs")))
        roots.extend(sorted(bins.glob("*/main.rs")))
    roots = [r for r in roots if r.is_file()]
    if not roots:
        return []
    reached: set[Path] = set()
    for root in roots:
        reached |= reachable(root)
    found = []
    for path in sorted(crate_src.rglob("*.rs")):
        if path in reached:
            continue
        # src/bin/*.rs is its own root, already collected above.
        if bins in path.parents:
            continue
        found.append(path)
    return found


# Pre-existing orphans, as paths relative to the repo root. Empty: the three
# stub files that were here were deleted, and a new orphan fails the gate.
_BASELINE: frozenset[str] = frozenset()


def check(root: Path = Path("crates")) -> list[Path]:
    """Every orphan under every crate's src/ that is not in the baseline."""
    out = []
    for src in sorted(root.glob("*/src")):
        for path in orphans(src):
            if path.as_posix() not in _BASELINE:
                out.append(path)
    return out


if __name__ == "__main__":
    found = check()
    baselined = [p for src in sorted(Path("crates").glob("*/src")) for p in orphans(src) if p.as_posix() in _BASELINE]
    if found:
        for path in found:
            print(
                f"error: {path.as_posix()} is not declared by any mod, so it "
                f"never compiles. Declare it, or delete it if its content "
                f"moved.",
                file=sys.stderr,
            )
        sys.exit(1)
    print(f"[orphan-modules] ok ({len(baselined)} pre-existing)")
    sys.exit(0)
