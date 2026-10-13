#!/usr/bin/env python3
"""Regression tests for the flat test peer gate (check_rust_naming rule 9).

The gate is the machine backstop for a layout rule the model applies
unreliably: a module's tests belong in X/tests.rs (declared as
#[cfg(test)] mod tests;), not in a sibling X_tests.rs reached through
#[path = "..._tests.rs"]. The directory form needs no attribute and mirrors
the source path; the flat form escapes the module system on every module it
covers. So the gate must be guarded itself (same reason test_flat_prefix.py
exists).

Each test is PAIRED: a positive control (the gate DOES fire on a new pair)
alongside the negative (an exempt shape does NOT count). A test that only
asserts the negative can pass for the wrong reason -- a scanner returning
nothing because of an unrelated path mismatch would pass any "not counted"
assertion -- so every exemption test mixes an exempt shape with a real pair
and asserts the real one is counted.

Covered logics:
  - a #[path = "..._tests.rs"] declaration counts toward the ratchet
  - the directory form (X.rs + X/tests.rs, mod tests;) does not count
  - integration tests (tests/) do not count
  - both baseline verdicts: tolerate the grandfathered count AND fail one
    over it (the injection contract -- _check_flat_test_peer must read the
    `baseline` parameter, not the module global)

Run: python3 scripts/test_flat_test_peer.py  (wired into make check as
flat-test-peer-tests). Exit 0 = pass, 1 = fail.
"""
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_rust_naming import _check_flat_test_peer  # noqa: E402

FLAT = '#[cfg(test)]\n#[path = "foo_tests.rs"]\nmod foo_tests;\n'
DIRECTORY = "#[cfg(test)]\nmod tests;\n"


def _make_tree(spec: dict[str, str]) -> Path:
    """Build a tmpdir tree. spec = {relpath_under_crate_src: file body}."""
    root = Path(tempfile.mkdtemp())
    for rel, body in spec.items():
        p = root / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body)
    return root


def _errs(root: Path, baseline: int) -> list[str]:
    e: list[str] = []
    _check_flat_test_peer(e, root=root, baseline=baseline)
    return e


def main() -> int:
    failures = []

    # 1. the flat form counts; the directory form does not (paired: the same
    # tree holds both, so a scanner that counted everything would report 2).
    root = _make_tree({
        "crate/src/flat.rs": FLAT,
        "crate/src/flat_tests.rs": "//! stub\n",
        "crate/src/dir.rs": DIRECTORY,
        "crate/src/dir/tests.rs": "//! stub\n",
    })
    errs = _errs(root, 0)
    if not any("flat.rs" in e and "flat test peers: 1" in e for e in errs):
        failures.append(f"flat form must count once, got {errs}")
    if any("dir.rs" in e for e in errs):
        failures.append(f"directory form must not count, got {errs}")

    # 2. integration tests are binaries, not modules: an X_tests.rs under
    # tests/ does not count, while the same shape under src/ does.
    root = _make_tree({
        "crate/tests/foo_tests.rs": "//! stub\n",
        "crate/tests/foo.rs": FLAT,
        "crate/src/bar.rs": FLAT,
    })
    errs = _errs(root, 0)
    if not any("bar.rs" in e and "flat test peers: 1" in e for e in errs):
        failures.append(f"src flat peer must count, got {errs}")
    if any("/tests/" in e for e in errs):
        failures.append(f"tests/ must be skipped, got {errs}")

    # 3. both baseline verdicts (the injection contract): at the grandfathered
    # count the gate passes, one over it fails. This pins that the check reads
    # the `baseline` parameter, not the module global -- if it read the global
    # (132), a tree with one pair would still be under it and never fire.
    root = _make_tree({"crate/src/foo.rs": FLAT})
    if _errs(root, 1):
        failures.append("at baseline: a grandfathered pair must be tolerated")
    errs = _errs(root, 0)
    if not any("flat test peers: 1 (baseline 0)" in e for e in errs):
        failures.append(f"over baseline: must fire, got {errs}")

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        print(f"\n[flat-test-peer-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[flat-test-peer-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
