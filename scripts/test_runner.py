#!/usr/bin/env python3
"""Shared test runner for the script-test gates.

Every gate script that runs a bundle of inline test_* functions prints the
same one-line verdict so make check's script-tests step is uniform: a single
`[<gate>-tests] ok` on success, or the failure list on failure. Collected
(not first-fail-stop) so one run surfaces every break, not just the earliest.
"""
import sys


def run(gate: str, scope: dict) -> int:
    """Run every test_* callable in `scope` (the caller's globals), collect
    failures, and print `[<gate>-tests] ok` or the failure list. Returns 0
    on success, 1 on any failure."""
    names = sorted(n for n in scope if n.startswith("test_"))
    failures = []
    for name in names:
        try:
            scope[name]()
        except Exception as e:  # noqa: BLE001 — collect, do not stop
            failures.append((name, e))
    if failures:
        print(f"[{gate}-tests] {len(failures)} failure(s):", file=sys.stderr)
        for name, e in failures:
            print(f"  {name}: {e}", file=sys.stderr)
        return 1
    print(f"[{gate}-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(0)
