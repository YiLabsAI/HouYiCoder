#!/usr/bin/env python3
"""Reject CJK, backtick-quoted identifiers, opaque labels, and document
references in .rs comments. Code comments must be plain English prose: bare
identifiers (no backtick markup), no Chinese, no short letter-and-digit
labels or numbered stage names standing in for a concept. See AGENTS.md
'Code comment style'.

Only .rs comment lines are scanned; prose documents are unaffected (they may
be Chinese, use backticks, and carry reference numbers).

The comment-rule detector is shared with the PreToolUse write-time hook
(hook_rust.py) via rules/comments.py — one word source, no parallel
wordlist to drift. commit_msg_lint.py does the same for commit time.

Exit 1 on any violation, else 0.
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from rules.comments import line_violations  # noqa: E402


def _rs_files(paths):
    if paths:
        return [Path(p) for p in paths if p.endswith(".rs")]
    return [p for p in Path("crates").rglob("*.rs")]


def main(paths):
    errors = []
    for p in _rs_files(paths):
        if not p.is_file():
            continue
        try:
            text = p.read_text(encoding="utf-8")
        except OSError:
            continue
        for lineno, line in enumerate(text.splitlines(), start=1):
            for message, _line_text in line_violations(line, check_cjk=False):
                # CJK is owned by check_no_cjk (cross-filetype); the other
                # 5 arms are .rs-comment-specific.
                errors.append(f"{p}:{lineno}: {message}")
    for e in errors:
        print(f"error: {e}", file=sys.stderr)
    if errors:
        print(
            f"\n[CommentStyle] {len(errors)} violation(s) in .rs comments.",
            file=sys.stderr,
        )
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
