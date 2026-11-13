#!/usr/bin/env python3
"""Regression tests for the platform PAL boundary gate.

The gate is the machine backstop for the rule that no production layer above
the platform carries an OS cfg. A test that only asserts the negative can pass
for the wrong reason -- a path-match failure returning no violations -- so each
test mixes a whitelisted crate (the platform, which may carry cfg) with a
non-whitelisted one and asserts the non-whitelished one is the only one flagged.

Run: python3 scripts/test_platform_pal.py  (wired into make check).
Exit 0 = pass, 1 = fail.
"""
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_platform_pal import WHITELIST, _ratchet_failures, check  # noqa: E402


def _make_tree(spec: dict[str, dict[str, str]]) -> Path:
    """Build a tmpdir tree of crates. spec = {crate_name: {file_relpath: text}}."""
    root = Path(tempfile.mkdtemp())
    for crate, files in spec.items():
        for rel, text in files.items():
            path = root / crate / "src" / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
    return root


def _violations(root: Path) -> list[str]:
    return check(root)


def test_whitelisted_crate_not_flagged() -> None:
    """The platform crate may carry an OS cfg; an upper crate may not."""
    root = _make_tree(
        {
            "houyicoder-platform": {"net.rs": "#[cfg(windows)]\npub fn f() {}\n"},
            "houyicoder-other": {"net.rs": "#[cfg(windows)]\npub fn f() {}\n"},
        }
    )
    v = _violations(root)
    assert "houyicoder-platform" in WHITELIST
    flagged = [line for line in v if "houyicoder-platform" in line]
    assert not flagged, "the platform crate is whitelisted"
    assert any("houyicoder-other" in line for line in v), "an upper crate is flagged"


def test_source_not_flagged() -> None:
    """A test file (tests.rs, a _tests.rs peer, or under tests/) is exempt."""
    root = _make_tree(
        {
            "houyicoder-other": {
                "tests.rs": "#[cfg(windows)]\npub fn t() {}\n",
                "mod_tests.rs": "#[cfg(unix)]\npub fn t() {}\n",
            }
        }
    )
    (root / "houyicoder-other" / "src" / "tests").mkdir(parents=True)
    (root / "houyicoder-other" / "src" / "tests" / "x.rs").write_text(
        "#[cfg(windows)]\npub fn t() {}\n"
    )
    assert not _violations(root), "test source is exempt"


def test_ratchet_catches_growth() -> None:
    """A crate past its pinned baseline fails; at or below it passes."""
    lines = [f"houyicoder-other/a.rs:{n}: #[cfg(windows)]" for n in range(3)]
    assert not _ratchet_failures(lines, {"houyicoder-other": 3}), "at the baseline passes"
    assert not _ratchet_failures(lines, {"houyicoder-other": 5}), "below the baseline passes"
    growth = _ratchet_failures(lines, {"houyicoder-other": 2})
    assert growth == {"houyicoder-other": 3}, "above the baseline fails with the count"


def main() -> int:
    tests = [
        test_whitelisted_crate_not_flagged,
        test_source_not_flagged,
        test_ratchet_catches_growth,
    ]
    failed = 0
    for test in tests:
        try:
            test()
        except AssertionError as error:
            print(f"FAIL: {test.__name__}: {error}", file=sys.stderr)
            failed += 1
    print(f"[platform-pal-tests] {len(tests) - failed}/{len(tests)} passed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
