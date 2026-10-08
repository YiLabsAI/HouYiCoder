#!/usr/bin/env python3
"""Struct field gates, driven by the monitored-struct list.

Per-owner: strict-pin the field count of every registry owner. Growth and
drift both block, and a registered owner that disappears (renamed or
deleted without re-registering) also blocks -- renaming cannot evade the
gate. The global raw field total is report-only during the migration:
splitting fields into wrappers grows it legitimately, so the total is
printed as a trend, never blocking, and never silently treated as green.
Baselines live in rules/monitored_structs.py and move only with a reviewed reason
in the same commit that changes reality.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.monitored_structs import ACTIVE_OWNERS  # noqa: E402
from report_structure_facts import (  # noqa: E402
    iter_struct_field_counts,
    struct_field_counts,
)

# The raw total from when the counter started seeing every declaration form
# present in the tree. Before that it matched only bare and pub declarations
# with no generic parameters, so the crate-internal owners a refactor
# introduces were invisible to it and could not be registered. The anchor the
# migration started from predates the fix and was measured with the
# incomplete pattern, so reading a trend across the two would report a change
# of measuring stick as a change of code. The trend restarts here.
GLOBAL_TOTAL_ANCHOR = 751


def evaluate_owner(actual, baseline):
    """Strict-pin per owner: 0 only when actual == baseline."""
    return 0 if actual == baseline else 1


def evaluate_registry(counts, registry=ACTIVE_OWNERS):
    """Run per-owner gates against a counts mapping. Returns error strings
    (empty == green). Pure; tested by test_struct_fields. A registered
    owner missing from counts is an error -- renaming or deleting without
    re-registering cannot go green. The global total is deliberately not
    gated here."""
    errors = []
    for key, cfg in registry.items():
        actual = counts.get(key)
        if actual is None:
            errors.append(
                f"registered owner {key} disappeared from the struct "
                "counts -- register the new fully-qualified name in "
                "rules/monitored_structs.py in the same commit"
            )
        elif actual != cfg["fields"]:
            kind = "grew" if actual > cfg["fields"] else "dropped"
            errors.append(
                f"{key} field count {kind}: {actual} != {cfg['fields']} -- "
                "move fields with the owner, or re-baseline with a "
                "reviewed reason in the same commit"
            )
    return errors


def main() -> int:
    # The registry reads every struct, not just the ones over the warn
    # floor: a small registered owner is still a pinned owner, and the
    # floor is a report filter, not an escape from the pin. The global
    # trend total keeps the filtered view it was baselined against.
    all_counts = dict(iter_struct_field_counts())
    errors = evaluate_registry(all_counts)
    total = sum(dict(struct_field_counts()).values())
    delta = total - GLOBAL_TOTAL_ANCHOR
    trend = f"+{delta}" if delta >= 0 else str(delta)
    if errors:
        for e in errors:
            print(f"error: {e}", file=sys.stderr)
        print(
            f"\n[struct-fields] {len(errors)} owner gate(s) red. Global "
            f"raw total {total} ({trend} since the counter saw every "
            "struct) is report-only during the migration.",
            file=sys.stderr,
        )
        return 1
    print(
        f"[struct-fields] {len(ACTIVE_OWNERS)} owner(s) green. Global raw "
        f"total {total} ({trend} since the counter saw every struct) "
        "-- report-only."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
