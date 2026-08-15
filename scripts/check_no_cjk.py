#!/usr/bin/env python3
"""No-CJK gate — the one comment rule that holds across all source filetypes.

The .rs comment gate (check_rs_comments.py) runs four arms (codename,
doc-ref, own-name, backtick) that all hold on .rs because .rs is
the CHECKED object. Porting them to .py gate-scripts failed (tested, then
dropped): the gate scripts must name what they forbid (codenames), scan
what they serve (docs/), and identify their subject (houyicoder) — so
those arms are noise on the checker itself.

CJK is the exception: no source filetype has a reason to carry Chinese in
comments or docstrings. This gate scans comment/docstring lines across
.rs / .py / .sh / Makefile for CJK. Zero false positives (the one prior
hit — a Chinese phrase in a .py docstring — was a real violation).

The CJK arm was removed from check_rs_comments (it defers to this gate
at L2). The write-time hook (hook_rust.py) keeps CJK at L1 for .rs.

Exit 1 on any CJK in a comment/docstring, else 0.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.comments import has_cjk, is_comment, py_doc_lines  # noqa: E402

REPO = Path(__file__).resolve().parent.parent


def _rs_comment_lines(path: Path):
    """Yield (lineno, line) for // comment lines in a .rs file."""
    try:
        for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            if is_comment(line):
                yield lineno, line
    except OSError:
        pass


def _py_comment_lines(path: Path):
    """Yield (lineno, line) for # comment + docstring lines in a .py file."""
    try:
        yield from py_doc_lines(path.read_text(encoding="utf-8"))
    except OSError:
        pass


def _hash_comment_lines(path: Path):
    """Yield (lineno, line) for # comment lines in a .sh / Makefile."""
    try:
        for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
            stripped = line.lstrip()
            if stripped.startswith("#") and not stripped.startswith("#!"):
                yield lineno, line
    except OSError:
        pass


def main() -> int:
    errors = []

    # .rs — crates/ (source code, not tests/)
    for p in (REPO / "crates").rglob("*.rs"):
        if "/target/" in str(p):
            continue
        for lineno, line in _rs_comment_lines(p):
            if has_cjk(line):
                errors.append(f"{p.relative_to(REPO)}:{lineno}: CJK in .rs comment")

    # .py — scripts/ (gate tooling). All .py scanned: comments, inline
    # comments, and docstrings via the stdlib tokenizer (rules/comments.py).
    for p in (REPO / "scripts").rglob("*.py"):
        if "/__pycache__/" in str(p):
            continue
        for lineno, line in _py_comment_lines(p):
            if has_cjk(line):
                errors.append(f"{p.relative_to(REPO)}:{lineno}: CJK in .py comment/docstring")

    # .sh + Makefile at repo root + scripts/
    for p in list(REPO.glob("*.sh")) + list((REPO / "scripts").rglob("*.sh")) + [REPO / "Makefile"]:
        if not p.is_file():
            continue
        for lineno, line in _hash_comment_lines(p):
            if has_cjk(line):
                errors.append(f"{p.relative_to(REPO)}:{lineno}: CJK in comment")

    for e in errors:
        print(f"error: {e}", file=sys.stderr)
    if errors:
        print(f"\n[NoCJK] {len(errors)} CJK occurrence(s) in comments/docstrings.", file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
