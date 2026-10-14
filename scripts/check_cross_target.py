#!/usr/bin/env python3
"""Cross-target compile ratchet: a cfg-gated module must compile everywhere.

A module behind `#[cfg(target_os = "linux")]` is invisible to a check that
compiles the host only, so an import can be dropped from it and every gate
stays green on the developer's machine while the crate stops building on
Linux. This check compiles the workspace libraries for each target that is
installed and counts the errors, as a ratchet: the count may only fall, so a
new break fails and fixing a pre-existing one lowers the baseline.

A target that is not installed is reported and skipped rather than failed:
the toolchain, not the code, decides what can be compiled here.
"""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

# Errors per target, as a ratchet. Linux is clean; the two windows errors are
# the pre-existing `std::os::unix` imports, awaiting a cfg gate of their own.
_TARGET_BASELINE = {
    "x86_64-unknown-linux-gnu": 0,
    "x86_64-pc-windows-msvc": 2,
}
# A rustc diagnostic carries a code (error[E0425]). A build script that cannot
# run for the target says "failed to run custom build command" and is a
# toolchain limit, not a finding: the dependency graph is not what this check
# guards, and counting it would fail every machine without a cross linker.
_ERROR_RE = re.compile(r"^error\[E\d+\]", re.M)
_BUILD_SCRIPT_RE = re.compile(r"failed to run custom build command")


def installed_targets() -> set[str]:
    """The targets the toolchain has, or an empty set when rustup is absent."""
    try:
        out = subprocess.run(
            ["rustup", "target", "list", "--installed"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return set()
    return set(out.stdout.split())


def gated_crates(root: Path) -> list[str]:
    """The crates whose source carries a platform cfg gate.

    Only those can hide a break from a host-only check, so only those are
    compiled per target. A workspace-wide check would also compile crates
    whose dependencies need a linker for the target, which fails for a reason
    that has nothing to do with the code.
    """
    out = []
    for manifest in sorted((root / "crates").glob("*/Cargo.toml")):
        src = manifest.parent / "src"
        if not src.is_dir():
            continue
        for path in src.rglob("*.rs"):
            text = path.read_text(encoding="utf-8")
            if "#[cfg(target_os" in text or "#[cfg(unix)" in text or "#[cfg(not(unix))" in text:
                out.append(manifest.parent.name)
                break
    return out


def error_count(crate: str, target: str, root: Path) -> int | None:
    """rustc diagnostics from a library-only check of one crate, or None when
    the target cannot be built here at all (a dependency's build script needs
    a linker the toolchain does not have).

    Libraries only: a bin or a test target needs a linker for the target,
    which is not what this check is about, and a cfg-gated module in a
    library is where this class of break lives.
    """
    out = subprocess.run(
        ["cargo", "check", "-p", crate, "--lib", "--target", target],
        capture_output=True,
        text=True,
        cwd=root,
    )
    if _BUILD_SCRIPT_RE.search(out.stderr) and not _ERROR_RE.search(out.stderr):
        return None
    return len(_ERROR_RE.findall(out.stderr))


def check(root: Path = Path(".")) -> int:
    """Report each target's error count against its baseline. Returns the
    number of targets over baseline."""
    installed = installed_targets()
    crates = gated_crates(root)
    over = 0
    for target, baseline in sorted(_TARGET_BASELINE.items()):
        if target not in installed:
            print(f"[cross-target] {target}: not installed, skipped")
            continue
        total, skipped = 0, 0
        for crate in crates:
            count = error_count(crate, target, root)
            if count is None:
                skipped += 1
                continue
            if count:
                print(f"  {crate}: {count} error(s) for {target}", file=sys.stderr)
            total += count
        if total > baseline:
            over += 1
            print(
                f"[cross-target] {target}: {total} error(s) > baseline "
                f"{baseline}. A module behind a cfg gate stopped compiling: "
                f"check the imports inside it. Run cargo check -p <crate> --lib "
                f"--target {target} to see them.",
                file=sys.stderr,
            )
        else:
            note = f", {skipped} crate(s) skipped" if skipped else ""
            print(f"[cross-target] {target}: {total} error(s), baseline {baseline}{note}")
    return over


if __name__ == "__main__":
    sys.exit(1 if check() else 0)
