#!/usr/bin/env python3
"""Regression tests for the flat-prefix module gate (check_rust_naming rule 8).

The gate is the machine backstop for a rule the model applies unreliably
post-compact -- "prefer a directory module to a flat-prefix pair" -- so the
gate itself must be guarded (same reason test_hook_rust.py exists).

Each test is PAIRED: a positive control (the gate DOES fire on a real pair,
or DOES tolerate a listed one) alongside the negative (an exempt shape does
NOT fire). A test that only asserts the negative can pass for the wrong
reason -- _flat_prefix_pairs returning [] because of an unrelated path
match failure would pass any "not flagged" assertion -- so every exemption
test mixes an exempt pair with a real pair and asserts the real one is the
ONLY one returned.

Covered logics:
  - longest parent stem (markdown_memory_io pairs with markdown_memory, not
    a nonexistent markdown)
  - _tests child exemption (foo_tests.rs is the peer-test convention)
  - _tests parent exemption (a parent ending in _tests is a test family)
  - tests/ exclusion (each tests/*.rs is its own binary, not a module)
  - both baseline verdicts: tolerate a listed pair AND fail a dead line
    (the injection contract -- _check_flat_prefix must read the `baseline`
    parameter, not the module global)

Run: python3 scripts/test_flat_prefix.py  (wired into make check as
flat-prefix-tests). Exit 0 = pass, 1 = fail.
"""
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_rust_naming import _check_flat_prefix, _flat_prefix_pairs  # noqa: E402


def _make_tree(spec: dict[str, list[str]]) -> Path:
    """Build a tmpdir tree of .rs files. spec = {dir_relpath: [stems]}."""
    root = Path(tempfile.mkdtemp())
    for rel, stems in spec.items():
        d = root / rel
        d.mkdir(parents=True, exist_ok=True)
        for s in stems:
            (d / f"{s}.rs").write_text("//! stub\n")
    return root


def _pairs(root: Path):
    src_dirs = [p.parent for p in root.rglob("*.rs")
                if "/src/" in str(p) and "/tests/" not in str(p)]
    return _flat_prefix_pairs(src_dirs)


def _errs(root: Path, baseline: frozenset[str]) -> list[str]:
    e: list[str] = []
    _check_flat_prefix(e, root=root, baseline=baseline)
    return e


def main() -> int:
    failures = []

    # 1. longest parent stem: markdown_memory_io.rs pairs with
    # markdown_memory.rs (which exists), not a nonexistent markdown.rs.
    root = _make_tree({"crate/src": ["markdown_memory", "markdown_memory_io"]})
    got = {(p, c) for _, p, c in _pairs(root)}
    if got != {("markdown_memory", "markdown_memory_io")}:
        failures.append(f"longest-stem: expected markdown_memory->markdown_memory_io only, got {got}")

    # 2. _tests child exemption + positive control: foo_tests.rs is the
    # peer-test convention (exempt); bar_baz.rs is a real pair (must fire).
    root = _make_tree({"crate/src": ["foo", "foo_tests", "bar", "bar_baz"]})
    got = {(p, c) for _, p, c in _pairs(root)}
    if got != {("bar", "bar_baz")}:
        failures.append(f"_tests child: expected only bar->bar_baz, got {got}")

    # 3. _tests parent exemption + positive control: a parent ending in
    # _tests is a test family (exempt); bar_baz is a real pair (must fire).
    root = _make_tree({"crate/src": ["foo_tests", "foo_tests_extra", "bar", "bar_baz"]})
    got = {(p, c) for _, p, c in _pairs(root)}
    if got != {("bar", "bar_baz")}:
        failures.append(f"_tests parent: expected only bar->bar_baz, got {got}")

    # 4. tests/ exclusion + positive control: tests/*.rs are binaries
    # (exempt); a src/ pair must still fire.
    root = _make_tree({
        "crate/src": ["foo", "foo_bar"],
        "crate/tests": ["seatbelt", "seatbelt_kernel"],
    })
    errs = _errs(root, frozenset())
    if not any("foo_bar.rs" in e and "flat-prefix module pair" in e for e in errs):
        failures.append(f"tests/ exclusion: src pair foo_bar must fire, got {errs}")
    if any("seatbelt_kernel" in e for e in errs):
        failures.append(f"tests/ exclusion: tests/ pair must be skipped, got {errs}")

    # 5. both baseline verdicts (the injection contract): a pair listed in
    # the INJECTED baseline is tolerated, a dead line is failed. This pins
    # that _check_flat_prefix reads the `baseline` parameter, not the module
    # global -- if it reads the global, the listed pair still errors.
    root = _make_tree({"crate/src": ["foo", "foo_bar"]})
    listed = (root / "crate/src/foo_bar.rs").as_posix()
    ghost = "crate/src/ghost.rs"
    errs = _errs(root, frozenset({listed, ghost}))
    if any("foo_bar" in e and "flat-prefix module pair" in e for e in errs):
        failures.append(
            f"injection: listed pair must be tolerated, got {[e for e in errs if 'foo_bar' in e]}"
        )
    if not any("ghost.rs" in e and "stale" in e for e in errs):
        failures.append(f"injection: dead baseline line must fail, got {errs}")

    # 6. new flat prefix not in baseline fails (the ratchet blocks a NEW
    # pair, tolerates only the listed stock) -- the negative half of 5.
    root = _make_tree({"crate/src": ["foo", "foo_bar"]})
    errs = _errs(root, frozenset())
    if not any("foo_bar.rs" in e and "flat-prefix module pair" in e for e in errs):
        failures.append(f"new flat-prefix: must fire, got {errs}")

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        print(f"\n[flat-prefix-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[flat-prefix-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
