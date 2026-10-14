#!/usr/bin/env python3
"""Import locality ratchet (L2): two counts that may only fall.

  * a use statement inside a function body -- it belongs at module scope, so
    the import block shows where the dependency enters.
  * an importable qualified path with two or more module segments, used
    without an import -- the reader meets a chain instead of the name
    once. Counted as one finding per file and chain, not per use.

Both are counted, not listed, because the backlog is large and the rule is
per-file. Growth blocks; lower a baseline in the commit that cleans files.
A name the file defines itself, or one that two chains share, stays
qualified: an import would collide or pick the wrong type.

The detector lives in rules/imports.py, shared with the write-time hook
(hook_rust.py) so the two cannot disagree about what a violation is.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.imports import inline_use_violations, repeated_deep_paths  # noqa: E402

# Both counted over crates/*/src. Lower each in the commit that cleans files.
_INLINE_USE_BASELINE = 203
_REPEATED_PATH_BASELINE = 1182
_INLINE_USE_RULE = (
    "a use statement inside a function body; move it to the module's import block"
)
_REPEATED_PATH_RULE = (
    "a deep qualified path without an import; import it once at module scope"
)


def scan(root: Path = Path("crates")):
    """Count both rule violations under root, with the worst files named.

    The deep-path count is one finding per file and chain: the rule is
    that a deep chain should be imported, not that each use is wrong.
    """
    inline, repeated = [], []
    for path in sorted(root.rglob("*.rs")):
        if "/target/" in path.as_posix():
            continue
        text = path.read_text(encoding="utf-8")
        for lineno, _ in inline_use_violations(text):
            inline.append(f"{path.as_posix()}:{lineno}")
        for chain, count in repeated_deep_paths(text).items():
            repeated.append(f"{path.as_posix()}: {chain} x{count}")
    return inline, repeated


def evaluate(
    inline,
    repeated,
    inline_baseline=_INLINE_USE_BASELINE,
    path_baseline=_REPEATED_PATH_BASELINE,
):
    """Pure: 0 when neither count grew. Pure so the regression test can lock
    both directions without scanning the tree."""
    return 0 if len(inline) <= inline_baseline and len(repeated) <= path_baseline else 1


def main() -> int:
    inline, repeated = scan()
    grew = []
    if len(inline) > _INLINE_USE_BASELINE:
        print(
            f"error: {_INLINE_USE_RULE}: {len(inline)} > {_INLINE_USE_BASELINE} "
            f"(+{len(inline) - _INLINE_USE_BASELINE}). A local import hides where "
            f"the dependency enters and the import block cannot show it.",
            file=sys.stderr,
        )
        grew.append(inline)
    if len(repeated) > _REPEATED_PATH_BASELINE:
        print(
            f"error: {_REPEATED_PATH_RULE}: {len(repeated)} > "
            f"{_REPEATED_PATH_BASELINE} (+{len(repeated) - _REPEATED_PATH_BASELINE}). "
            f"Import the chain once at module scope.",
            file=sys.stderr,
        )
        grew.append(repeated)
    for listing in grew:
        for where in listing[:8]:
            print(f"  {where}", file=sys.stderr)
    return evaluate(inline, repeated)


if __name__ == "__main__":
    sys.exit(main())
