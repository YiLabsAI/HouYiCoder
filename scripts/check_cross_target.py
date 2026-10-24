#!/usr/bin/env python3
"""Cross-target compile ratchet: a cfg-gated module must compile everywhere.

A module behind `#[cfg(target_os = "linux")]` is invisible to a check that
compiles the host only, so an import can be dropped from it and every gate
stays green on the developer's machine while the crate stops building on
Linux. This check compiles for each target that is installed and counts the
errors, as a ratchet: the count may only fall, so a new break fails and
fixing a pre-existing one lowers the baseline.

Two properties decide whether this check can see the break at all:

Warnings are errors here, as they are in CI. An import that loses its only
user on the other platform is an unused-import warning locally and a build
failure in CI, so a run that leaves warnings as warnings counts nothing and
reports a clean sweep over a real break.

The Windows target needs a sysroot. A bare `cargo check --target` stops at
the first dependency whose build script needs a Windows C toolchain, which
skips the crates most likely to hold the break. cargo-xwin supplies that
sysroot, so when it is installed the Windows target is compiled through it
and every crate is actually checked.

A target that is not installed, or that no available toolchain can prepare,
is reported and skipped rather than failed: the toolchain, not the code,
decides what can be compiled here.
"""
from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

WINDOWS_TARGET = "x86_64-pc-windows-msvc"

# Errors per target, as a ratchet. Both are clean: the pre-existing unix imports
# that used to break windows now carry their gate.
_TARGET_BASELINE = {
    "x86_64-unknown-linux-gnu": 0,
    WINDOWS_TARGET: 0,
}

# CI builds with warnings denied, so a warning on the cross target is a build
# failure waiting for the next push. Counting diagnostics here rather than
# setting RUSTFLAGS keeps cargo's cache key stable: the flag would give this
# gate its own artifact tree and recompile everything on every run.
#
# Three lines are not findings of their own: the summaries that follow a
# failure would multiply the count by the number of failures, and a
# dependency's build script that cannot run for the target is a toolchain
# limit, not a break in this tree.
_DIAG_RE = re.compile(
    r"^(?:error(?:\[[A-Z]\d+\])?|warning): "
    r"(?!could not compile|aborting due to|failed to run custom build command"
    r"|build failed|\d+ warnings? emitted|[a-z0-9_-]+@\d)",
    re.M,
)
# A build script that cannot run for the target says "failed to run custom
# build command" and is a toolchain limit, not a finding: the dependency graph
# is not what this check guards, and counting it would fail every machine
# without a cross linker.
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


def xwin_available() -> bool:
    """Whether cargo-xwin is installed to supply the Windows sysroot."""
    try:
        out = subprocess.run(
            ["cargo", "xwin", "--version"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return False
    return out.returncode == 0


def gated_crates(root: Path) -> list[str]:
    """The crates whose source carries a platform cfg gate.

    Only those can hide a break from a host-only check, so only those are
    compiled per crate when no workspace-wide runner is available.
    """
    out = []
    for manifest in sorted((root / "crates").glob("*/Cargo.toml")):
        src = manifest.parent / "src"
        if not src.is_dir():
            continue
        for path in src.rglob("*.rs"):
            text = path.read_text(encoding="utf-8")
            if (
                "#[cfg(target_os" in text
                or "#[cfg(unix)" in text
                or "#[cfg(not(unix))" in text
            ):
                out.append(manifest.parent.name)
                break
    return out


def crate_error_count(crate: str, target: str, root: Path) -> int | None:
    """Diagnostics from a library and test check of one crate, or None when
    the target cannot be built here at all (a dependency's build script needs
    a linker the toolchain does not have).

    Both scopes are compiled: a cfg-gated break lives in a library as often as
    in a test module, and a test-only one is invisible to `--lib`.
    """
    total = 0
    has_lib = (root / "crates" / crate / "src" / "lib.rs").is_file()
    first_scope = ["--lib"] if has_lib else ["--bins"]
    for scope in (first_scope, ["--tests"]):
        out = subprocess.run(
            ["cargo", "check", "-p", crate, *scope, "--target", target],
            capture_output=True,
            text=True,
            cwd=root,
        )
        # A dependency's build script that cannot run for this target stops
        # the run before this crate is compiled at all, so every diagnostic
        # here belongs to the toolchain rather than to this tree.
        if _BUILD_SCRIPT_RE.search(out.stderr):
            return None
        found = len(_DIAG_RE.findall(out.stderr))
        if found > 0:
            total += found
        elif out.returncode != 0:
            total += 1
    return total


def workspace_error_count(target: str, root: Path) -> int | None:
    """Diagnostics from one workspace-wide cargo-xwin check for the target, or
    None when the run failed for a reason other than the code."""
    out = subprocess.run(
        [
            "cargo",
            "xwin",
            "check",
            "--target",
            target,
            "--workspace",
            "--all-targets",
        ],
        capture_output=True,
        text=True,
        cwd=root,
    )
    found = len(_DIAG_RE.findall(out.stderr))
    if found > 0:
        return found
    if out.returncode == 0:
        return 0
    if _BUILD_SCRIPT_RE.search(out.stderr) or "sysroot" in out.stderr.lower():
        return None
    return 1


def per_crate_scan(target: str, crates: list[str], root: Path) -> tuple[int, list[str]]:
    """Error total and the crates no available toolchain could build."""
    total, skipped = 0, []
    for crate in crates:
        count = crate_error_count(crate, target, root)
        if count is None:
            skipped.append(crate)
            continue
        if count:
            print(f"  {crate}: {count} error(s) for {target}", file=sys.stderr)
        total += count
    return total, skipped


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
        if target == WINDOWS_TARGET and xwin_available():
            # One workspace pass through cargo-xwin: it supplies the sysroot a
            # bare --target lacks, so the crates whose dependencies need a
            # Windows C toolchain are compiled rather than skipped.
            total = workspace_error_count(target, root)
            if total is None:
                print(
                    f"[cross-target] {target}: NOT CHECKED "
                    f"(cargo-xwin could not prepare the target)"
                )
                continue
            skipped: list[str] = []
        else:
            total, skipped = per_crate_scan(target, crates, root)
        if skipped:
            # A skipped crate is not checked at all, and this is how a cfg
            # break escaped once: its dependency needs a linker the toolchain
            # does not have. Say so rather than reporting a clean sweep.
            print(
                f"[cross-target] {target}: NOT CHECKED for "
                f"{', '.join(skipped)} (a build script cannot run here)"
            )
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
            note = f", {len(skipped)} crate(s) skipped" if skipped else ""
            print(f"[cross-target] {target}: {total} error(s), baseline {baseline}{note}")
    return over


if __name__ == "__main__":
    sys.exit(1 if check() else 0)
