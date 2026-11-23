#!/usr/bin/env python3
"""Unit-test isolation gate: keep cross-layer wiring out of src/.

The commit gate runs cargo test --lib, so a test written beside its owner
runs on every commit. That tier is for one unit with its collaborators
mocked at the bottom. A test that assembles the host, pairs a server, or
imports a local-endpoint stub is an integration test, and it costs every
commit while it sits in src/.

The scan reads every file under crates/*/src, not only the files a
naming convention calls a test. A #[cfg(test)] helper module carries no
such name, and that is exactly where the host assembly came back last
time.

One rule fails outright, and three ratchets hold the existing stock:

  1. A crate that does not own the composition root may not assemble the
     host: no houyicoder_service reference together with a build_runner
     entry point (the fresh and resume builders all match), and no
     start_local_server call. The composition root and the binary entry
     own both.
  2. A frontend src/ file may reference the host crate only up to the
     pinned count.
  3. A crate's src/ may import the local-endpoint stub only up to the
     pinned count.
  4. A crate's src/ may reference a workspace crate outside its runtime
     dependency whitelist only up to the pinned count.

A count at the pin passes and one past it fails, so the stock is removed
incrementally and never grows. The baseline is lowered in the change that
removes a file.

Run: python3 scripts/check_unit_isolation.py
     python3 scripts/check_unit_isolation.py --write-baseline
"""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BASELINE = Path(__file__).resolve().parent / "unit_isolation_baseline.json"

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_dep_graph import WHITELIST  # noqa: E402
from rules.paths import rs_source_files  # noqa: E402

# Crates whose test doubles are the sanctioned way to mock the bottom: an
# in-memory backend, a scripted provider, a session store over that backend.
# Naming one is not counted as a cross-layer reference anywhere under src,
# production or test, because these are the leaves a unit assembles its own
# collaborators from.
BOTTOM_CRATES = frozenset(
    {
        "houyicoder-api",
        "houyicoder-async",
        "houyicoder-config",
        "houyicoder-context",
        "houyicoder-memory",
        "houyicoder-platform",
        "houyicoder-protocol",
        "houyicoder-provider",
        "houyicoder-resilience",
        "houyicoder-session",
    }
)

# The crates that own the composition root: they define and use the host
# assembly, so their own source may name it.
PAIRING_OWNERS = frozenset({"houyicoder-service", "houyicoder-cli"})

HOST_CRATE = "houyicoder_service"
CRATE_REF = re.compile(r"\b(houyicoder_[a-z_]+)::")
WIREMOCK = re.compile(r"\bwiremock\b")
START_LOCAL_SERVER = re.compile(r"\bstart_local_server\b")
BUILD_RUNNER = re.compile(r"\bbuild_runner\w*\b")


def baseline() -> dict:
    """The pinned stock, or empty when the file is absent."""
    try:
        return json.loads(BASELINE.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


def source_files(root: Path):
    """Yield (crate, relative path, text) for every file under src/."""
    crates = root / "crates"
    for path in rs_source_files(root):
        parts = path.relative_to(crates).parts
        if len(parts) < 2 or parts[1] != "src":
            continue
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        yield parts[0], path.relative_to(root).as_posix(), text


def scan(root: Path = ROOT) -> tuple[list[str], dict[str, dict[str, int]]]:
    """Return (hard violations, per-rule per-crate counts).

    The root is a parameter so the self-test can state each rule over a
    throwaway tree instead of writing probe files into this one.
    """
    hard: list[str] = []
    counts: dict[str, dict[str, int]] = {
        "host_in_src": {},
        "wiremock_in_src": {},
        "cross_layer_files": {},
    }
    for crate, rel, text in source_files(root):
        if crate not in PAIRING_OWNERS:
            names_host = HOST_CRATE in text
            if names_host and BUILD_RUNNER.search(text):
                hard.append(f"{rel}: a src/ file assembles the host runner")
            if START_LOCAL_SERVER.search(text):
                hard.append(f"{rel}: a src/ file pairs a server through the host helper")
        if crate == "houyicoder-tui" and HOST_CRATE in text:
            counts["host_in_src"][crate] = counts["host_in_src"].get(crate, 0) + 1
        if WIREMOCK.search(text):
            counts["wiremock_in_src"][crate] = counts["wiremock_in_src"].get(crate, 0) + 1
        allowed = {c.replace("-", "_") for c in WHITELIST.get(crate, set())}
        allowed.add(crate.replace("-", "_"))
        allowed |= {c.replace("-", "_") for c in BOTTOM_CRATES}
        named = {m.group(1) for m in CRATE_REF.finditer(text)}
        if named - allowed:
            counts["cross_layer_files"][crate] = (
                counts["cross_layer_files"].get(crate, 0) + 1
            )
    return hard, counts


def growth(pinned: dict, found: dict) -> dict[str, tuple[int, int]]:
    """Crates whose count grew past the pinned stock."""
    return {
        crate: (count, pinned.get(crate, 0))
        for crate, count in found.items()
        if count > pinned.get(crate, 0)
    }


def main() -> int:
    hard, counts = scan()
    pinned = baseline()
    if "--write-baseline" in sys.argv:
        BASELINE.write_text(
            json.dumps(counts, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        print(f"[unit-isolation] baseline written: {BASELINE.name}")
        return 0

    failures = list(hard)
    labels = {
        "host_in_src": "host reference in the frontend",
        "wiremock_in_src": "local-endpoint stub import",
        "cross_layer_files": "cross-layer reference",
    }
    for key, label in labels.items():
        for crate, (count, cap) in sorted(growth(pinned.get(key, {}), counts[key]).items()):
            failures.append(
                f"{crate}: {count} src/ files with a {label} "
                f"(baseline {cap}); mock the collaborator or move the test to tests/"
            )

    if failures:
        print("[unit-isolation] violations:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        print(
            "\nA unit test mocks its collaborators; a test that drives a real "
            "server, serve loop, or local endpoint belongs in tests/.",
            file=sys.stderr,
        )
        return 1
    print("[unit-isolation] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
