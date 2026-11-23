#!/usr/bin/env python3
"""Tests for the unit-test isolation gate.

Each rule is exercised in both directions over a throwaway tree: a clean
layout passes, and the shape the rule forbids fails. The scan also runs over
the real tree, so a rule that stops matching (a renamed helper, a moved
module) shows up here rather than in a commit that silently passes.

The helper-module case is the regression this file exists for: a
#[cfg(test)] module carries no test-shaped file name, so a scan that
filtered by name never read it, and the host assembly came back there.
"""

import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "scripts"))

from test_runner import run  # noqa: E402
import check_unit_isolation as gate  # noqa: E402


def probe(name: str, body: str, crate: str = "houyicoder-tui") -> tuple[list[str], dict]:
    """Scan a throwaway tree holding one src/ file with this body."""
    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw)
        path = root / "crates" / crate / "src" / name
        path.parent.mkdir(parents=True)
        path.write_text(body, encoding="utf-8")
        return gate.scan(root)


def test_clean_tree_passes() -> None:
    """The committed tree is inside the pinned stock."""
    hard, counts = gate.scan()
    assert not hard, f"the tree carries a hard violation: {hard}"
    pinned = gate.baseline()
    for key, found in counts.items():
        assert not gate.growth(pinned.get(key, {}), found), (
            f"{key} grew past the pin: {found}"
        )


def test_helper_module_is_scanned() -> None:
    """A cfg(test) helper module is read even though its name is not a test."""
    _, counts = probe(
        "test_harness.rs", "use houyicoder_service::server::Server;\n"
    )
    assert counts["host_in_src"].get("houyicoder-tui") == 1, counts


def test_host_assembly_fails() -> None:
    """A non-owner src/ file that assembles the host fails outright."""
    hard, _ = probe(
        "test_harness.rs",
        "use houyicoder_service::composition::{build_runner, BuildRunnerOptions};\n"
        "fn f() { let _ = build_runner(BuildRunnerOptions::default()); }\n",
    )
    assert any("assembles the host runner" in item for item in hard), hard


def test_start_local_server_fails() -> None:
    """A non-owner src/ file that pairs a server fails outright."""
    hard, _ = probe("tests.rs", "fn f() { start_local_server(); }\n")
    assert any("pairs a server" in item for item in hard), hard


def test_owner_may_assemble() -> None:
    """The composition root and the binary entry own the assembly."""
    hard, _ = probe(
        "tests.rs",
        "fn f() { let _ = houyicoder_service::composition::build_runner(); }\n",
        crate="houyicoder-service",
    )
    assert hard == [], hard


def test_local_build_runner_helper_passes() -> None:
    """A crate-local helper of that name is not the host assembly."""
    hard, _ = probe("tests.rs", "fn build_runner() {} fn f() { build_runner(); }\n")
    assert hard == [], hard


def test_wiremock_counts() -> None:
    """A src/ file that imports the local-endpoint stub is counted."""
    _, counts = probe("tests.rs", "fn f() { let _ = wiremock::MockServer::start(); }\n")
    assert counts["wiremock_in_src"].get("houyicoder-tui") == 1, counts


def test_growth_reads_the_cap() -> None:
    """A count at the cap is not growth; one past it is."""
    assert gate.growth({"a": 2}, {"a": 2}) == {}
    assert gate.growth({"a": 2}, {"a": 3}) == {"a": (3, 2)}


if __name__ == "__main__":
    raise SystemExit(run("unit-isolation", globals()))
