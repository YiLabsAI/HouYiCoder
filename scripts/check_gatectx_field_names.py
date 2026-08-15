#!/usr/bin/env python3
"""GateCtx field whitelist: reject fields whose name carries a containment,
sandbox, fenced, or network stem, so GateCtx cannot silently grow a
containment handle or a network projection again.

The containment contracts (C1/C3/C4) are structural: GateCtx carries no
containment field, no network-allowed flag, no sandbox handle. A future
edit that adds one -- even with a different name -- is caught here by
stem matching, so the structural guarantee does not depend on a code
review noticing it.

Only GateCtx is scanned, not DefaultModeGate. The gate struct
legitimately holds the Containment handle + auto_allow_fenced_exec
switch: it is the post_transform authority that uses them AFTER the
pipeline decides. The contract is that the PIPELINE (validators) cannot
see the fence state, and the pipeline reads GateCtx -- so GateCtx is
the struct that must stay clean.

The scan reads the GateCtx struct definition in pipeline/mod.rs, extracts
field names, and checks each against the blocklist.

Run: python3 scripts/check_gatectx_field_names.py  (wired into make check)
Exit 0 = pass, 1 = violation found.
"""
from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Stems that must never appear in a GateCtx field name. Adding one would
# let the pipeline read the fence state, making the gate a second authority
# over containment -- the exact shape contracts C1/C3/C4 dismantle.
BLOCKED_STEMS = ("containment", "sandbox", "fenced", "network")

# The struct whose fields are whitelisted. Only GateCtx -- the per-decision
# context the pipeline validators read. DefaultModeGate is NOT scanned: it
# holds the Containment handle + auto_allow switch for post_transform, which
# runs after the pipeline and is the authority the contract designates.
TARGETS = [
    ("GateCtx", ROOT / "crates/houyicoder-permission/src/pipeline/mod.rs"),
]


def extract_field_names(struct_name: str, path: Path) -> list[str]:
    """Return the field names of a struct by scanning its source file.

    Finds `pub struct <name> {` (or `struct <name> {`), then collects
    field names until the matching closing brace. A field name is the
    identifier before the colon on a line that is not a comment or blank.
    """
    text = path.read_text(encoding="utf-8")
    # Find the struct definition line.
    pattern = rf"(?:pub )?struct {re.escape(struct_name)} \{{"
    m = re.search(pattern, text)
    if m is None:
        return []
    # Collect field names until the closing brace at column 0 (the struct's
    # closing brace, not a nested one -- field types are simple enough that
    # nested braces are rare; when they occur the line still starts with the
    # field name before the colon).
    fields: list[str] = []
    for line in text[m.end():].splitlines():
        stripped = line.strip()
        if stripped == "}":
            break
        if stripped.startswith("//") or stripped.startswith("///") or not stripped:
            continue
        # A field line looks like:  pub field_name: Type,
        # or:                       field_name: Type,
        fm = re.match(r"(?:pub(?:\([^)]*\))?\s+)?([a-z_][a-z0-9_]*)\s*:", stripped)
        if fm:
            fields.append(fm.group(1))
    return fields


def main() -> int:
    violations: list[str] = []
    for struct_name, path in TARGETS:
        fields = extract_field_names(struct_name, path)
        for f in fields:
            for stem in BLOCKED_STEMS:
                if stem in f.lower():
                    violations.append(
                        f"{path.relative_to(ROOT)}: {struct_name} field '{f}' "
                        f"contains blocked stem '{stem}'. A field carrying a "
                        f"containment/sandbox/fenced/network stem lets the "
                        f"pipeline read the fence state, which contracts "
                        f"C1/C3/C4 dismantle. Remove it."
                    )
    if violations:
        for v in violations:
            print(f"error: {v}", file=sys.stderr)
        print(
            f"\n[gatectx-field-names] {len(violations)} violation(s).",
            file=sys.stderr,
        )
        return 1
    print("[gatectx-field-names] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
