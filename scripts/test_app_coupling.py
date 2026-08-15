#!/usr/bin/env python3
"""Regression test for measure_app_coupling: the default-include / explicit-
exclude method that the flawed one-off script got wrong.

The flawed method only counted cluster-dict fields, silently dropping
unknowns (method calls, unclustered fields). The correct method collects
EVERY app.<ident>/self.<ident>, then subtracts the cluster. The residue
(methods + non-cluster fields) is what blocks narrowing -- it must not
be silently dropped.

Tests the pure categorize_touches function with synthetic fn bodies.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from measure_app_coupling import categorize_touches, CLUSTER_FIELDS


def main() -> int:
    failures = []

    # 1. Only cluster fields -> residue empty -> narrows.
    c, r = categorize_touches("app.rules_cache.push(x); app.permission_cursor += 1;")
    if r or not c:
        failures.append(f"cluster-only: expected residue empty + cluster hit, got c={c} r={r}")

    # 2. Cluster field + App method call -> residue = {system_line} -> blocked.
    c, r = categorize_touches("app.system_line('msg'); app.rules_cache.push(x);")
    if "system_line" not in r:
        failures.append(f"method call must be in residue (default-include), got r={r}")
    if "rules_cache" not in c:
        failures.append(f"cluster field must be in cluster_hits, got c={c}")

    # 3. Cluster field + unclustered field -> residue = {pane} -> blocked.
    #    The flawed method DROPPED pane (not in cluster dict) -> false solo.
    c, r = categorize_touches("app.permission_tab = x; app.pane = y;")
    if "pane" not in r:
        failures.append(f"unclustered field must be in residue, got r={r}")
    if "permission_tab" not in c:
        failures.append(f"cluster field in cluster_hits, got c={c}")

    # 4. self. receiver also caught (not just app.).
    c, r = categorize_touches("self.mode_cache = None; self.screen = x;")
    if "mode_cache" not in c or "screen" not in r:
        failures.append(f"self. receiver: c={c} r={r}")

    # 5. Nothing touched -> both empty.
    c, r = categorize_touches("let x = 1 + 2;")
    if c or r:
        failures.append(f"no touches: expected both empty, got c={c} r={r}")

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        return 1
    print("app-coupling: default-include + explicit-exclude + method-call + unclustered ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
