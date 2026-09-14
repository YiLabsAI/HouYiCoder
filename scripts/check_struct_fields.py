#!/usr/bin/env python3
"""Struct field gates, driven by the monitored-struct list.

Per-owner: strict-pin the field count of every registry owner. Growth and
drift both block, and a registered owner that disappears (renamed or
deleted without re-registering) also blocks -- renaming cannot evade the
gate. The global raw field total is report-only during the migration:
splitting fields into wrappers grows it legitimately, so the total is
printed as a trend, never blocking, and never silently treated as green.
Baselines live in owner_registry.py and move only with a reviewed reason
in the same commit that changes reality.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.monitored_structs import ACTIVE_OWNERS  # noqa: E402
from report_structure_facts import struct_field_counts  # noqa: E402

# The real raw total at migration start, measured right after the parser
# fix. Report-only trend reference, never blocking.
GLOBAL_TOTAL_AT_MIGRATION_START = 582


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
                "owner_registry.py in the same commit"
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
    counts = dict(struct_field_counts())
    errors = evaluate_registry(counts)
    total = sum(counts.values())
    delta = total - GLOBAL_TOTAL_AT_MIGRATION_START
    trend = f"+{delta}" if delta >= 0 else str(delta)
    if errors:
        for e in errors:
            print(f"error: {e}", file=sys.stderr)
        print(
            f"\n[struct-fields] {len(errors)} owner gate(s) red. Global "
            f"raw total {total} ({trend} vs migration start) is "
            "report-only during the migration.",
            file=sys.stderr,
        )
        return 1
    print(
        f"[struct-fields] {len(ACTIVE_OWNERS)} owner(s) green. Global raw "
        f"total {total} ({trend} vs migration start) -- report-only."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
