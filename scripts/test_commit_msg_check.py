#!/usr/bin/env python3
"""Regression tests for commit_msg_check.sh, the structural commit gate.

The gate is the last layer of the commit discipline, so it carries its own
tests the way the other gates do. Every subject that must be blocked sits
beside the lookalike that must pass, so a pattern that accepts everything
fails as loudly as one that rejects everything.

The scope cases are the reason this file exists. A scope-less subject once
read as valid because the pattern made the scope optional, and two commits
reached the log that way; the guide and the gate agreed with each other and
disagreed with the standing rule, so nothing caught it. A concern scope and
a comma list are the sanctioned answers to "which module is this", and both
have a passing case here so the rule stays usable rather than just strict.

Each case runs in a temporary directory initialised as a repository of its
own. The gate finds the content lint it chains and an optional local wordlist
by asking git for the repository root, so a temporary directory that merely
happens to sit inside one would chain that repository's lint into every case
and turn this file into a test of whatever wordlist the enclosing checkout
carries.

Run: python3 scripts/test_commit_msg_check.py  (make check runs it).
Exit 0 = pass, 1 = fail.
"""
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "commit_msg_check.sh"


def _check(msg: str) -> int:
    """Run the structural gate on one message, in a repository holding no lint."""
    with tempfile.TemporaryDirectory() as tmp:
        subprocess.run(["git", "init", "-q", tmp], check=True, capture_output=True)
        path = Path(tmp) / "msg.txt"
        path.write_text(msg, encoding="utf-8")
        environ = {
            key: value
            for key, value in os.environ.items()
            if key != "COMMIT_LINT_WORDS"
        }
        return subprocess.run(
            ["bash", str(SCRIPT), str(path)],
            capture_output=True,
            text=True,
            cwd=tmp,
            env=environ,
        ).returncode


# A subject that must pass, beside the lookalike that must not.
PASS = [
    "feat(tui): add the pane\n",
    "fix(cli,tui): correct the row\n",
    "refactor(naming): rename the fold\n",
    "build(deps): bump the lockfile\n",
    "docs(guide): state the scope rule\n",
    # A subject of exactly 72 chars is inside the limit.
    "feat(tui): " + "x" * 61 + "\n",
    # A body wrapped at the limit passes.
    "feat(tui): add the pane\n\n" + "y" * 72 + "\n",
]

BLOCK = [
    # The recurrence this file guards: no scope at all.
    ("feat: add the pane\n", "a scope-less subject"),
    ("test: cover the fold\n", "a scope-less subject of another type"),
    ("feat(): add the pane\n", "an empty scope"),
    ("feat(TUI): add the pane\n", "an uppercase scope"),
    ("feat(tui):add the pane\n", "no space after the colon"),
    ("feat(tui) : add the pane\n", "a space before the colon"),
    ("wip(tui): add the pane\n", "a type outside the list"),
    ("feat(tui):\n", "no subject after the colon"),
    # One char over the subject limit.
    ("feat(tui): " + "x" * 62 + "\n", "a 73-char subject"),
    # One char over the body wrap.
    ("feat(tui): add the pane\n\n" + "y" * 73 + "\n", "a body line over 72"),
]


def main() -> int:
    failures = []

    for msg in PASS:
        if _check(msg) != 0:
            failures.append(f"blocked a message that must pass: {msg.splitlines()[0]!r}")

    for msg, why in BLOCK:
        if _check(msg) == 0:
            failures.append(f"accepted {why}: {msg.splitlines()[0]!r}")

    if failures:
        for failure in failures:
            print(f"FAIL: {failure}", file=sys.stderr)
        print(f"\n[commit-msg-check-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print(f"[commit-msg-check-tests] ok ({len(PASS)} pass, {len(BLOCK)} block)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
