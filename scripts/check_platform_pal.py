#!/usr/bin/env python3
"""Platform PAL boundary gate.

The platform crate (houyicoder-platform) is the only production layer that
may carry an operating-system cfg or a platform FFI dependency. Every other
crate holds no platform conditionals of its own: it asks the platform for
the answer, so a reader of any upper layer sees one boundary rather than a
scatter of cfg fragments that drift.

This gate scans each crate's production source for an OS cfg -- target_os,
unix, or windows -- and fails when one appears outside the platform crate or
the transitional sandbox crate (which still carries its own Windows job FFI
until the native launcher retires it). Test source is exempt: a test may
construct any platform shape.

Run: python3 scripts/check_platform_pal.py   (wired into make check)
Exit 0 = clean, 1 = a non-whitelisted crate carries an OS cfg.
"""
import json
import re
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CRATES = ROOT / "crates"
BASELINE = Path(__file__).resolve().parent / "platform_pal_baseline.json"

# Crates permitted to carry an OS cfg in production source. The platform owns
# the boundary; the sandbox is transitional, carrying Windows job FFI until the
# native launcher retires it.
WHITELIST: frozenset[str] = frozenset({"houyicoder-platform", "houyicoder-sandbox"})

# An OS cfg in attribute or macro position. The char class stops at the first
# close paren, so a nested cfg(not(unix)) still reads its inner flag.
CFG_OS = re.compile(r"cfg!?\([^)]*\b(?:target_os|unix|windows)\b")


def _is_production(path: Path) -> bool:
    """Whether a .rs file is production source the gate reads.

    Tests live under a tests/ directory, beside their owner as a *_tests.rs
    peer, or as a tests.rs module file (the #[cfg(test)] mod tests; peer in
    directory form); all three are exempt so a test that constructs a platform
    shape does not read as a layering leak.
    """
    parts = path.parts
    if "tests" in parts:
        return False
    name = path.name
    if name.endswith("_tests.rs") or name == "tests.rs":
        return False
    return True


def _scan_crate(crate_dir: Path, root: Path) -> list[str]:
    """The OS-cfg lines one crate's production source carries. Each line is
    tagged with the crate name as its first segment so the ratchet can group by
    crate without parsing the full path."""
    violations: list[str] = []
    src = crate_dir / "src"
    if not src.is_dir():
        return violations
    for path in sorted(src.rglob("*.rs")):
        if not _is_production(path):
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        rel = path.relative_to(src)
        for number, line in enumerate(text.splitlines(), start=1):
            if CFG_OS.search(line):
                violations.append(f"{crate_dir.name}/{rel}:{number}: {line.strip()}")
    return violations


def check(root: Path = CRATES) -> list[str]:
    """The OS-cfg violations across every non-whitelisted crate."""
    violations: list[str] = []
    for crate_dir in sorted(p for p in root.iterdir() if p.is_dir()):
        if crate_dir.name in WHITELIST:
            continue
        violations.extend(_scan_crate(crate_dir, root))
    return violations


def _ratchet_failures(violations: list[str], baseline: dict[str, int]) -> dict[str, int]:
    """The crates whose OS-cfg count grew past the pinned baseline."""
    counts = Counter(v.split("/", 1)[0] for v in violations)
    return {
        crate: counts[crate]
        for crate in counts
        if counts[crate] > baseline.get(crate, 0)
    }


def main() -> int:
    violations = check()
    # The ratchet: a per-crate count of the OS-cfg backlog the platform
    # migration has not yet cleared. The gate fails only when a crate grows
    # past its pinned count, so new leaks are caught while the existing
    # backlog is cleaned up incrementally. A shrink is an improvement that
    # passes; the baseline file is lowered in the same commit that removes one.
    baseline = json.loads(BASELINE.read_text(encoding="utf-8"))
    growth = _ratchet_failures(violations, baseline)
    if not growth:
        print("[platform-pal] ok")
        return 0
    print("[platform-pal] OS cfg grew past the baseline:", file=sys.stderr)
    for crate, count in sorted(growth.items()):
        pinned = baseline.get(crate, 0)
        print(
            f"  {crate}: {count} (baseline {pinned}); a new OS cfg leaked above the platform",
            file=sys.stderr,
        )
        for line in violations:
            if line.split("/", 1)[0] == crate:
                print(f"    {line}", file=sys.stderr)
    print(
        "  A production layer above the platform must not carry an OS cfg; "
        "ask the platform for the answer instead.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main())
