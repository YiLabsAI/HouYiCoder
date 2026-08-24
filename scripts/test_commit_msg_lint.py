#!/usr/bin/env python3
"""Regression tests for commit_msg_lint.py.

Pins two behaviors the product-name arm introduced:
  - COMPARISON framing ("surpasses") in the commit body is blocked, even
    with no product wordlist (COMPARISON is tracked, so the test is
    clone-safe).
  - The scissors line and the commented diff block below it are stripped
    before linting, so a commit that erases a product name from a .rs
    comment is not tripped by its own removed line (git commit -v appends
    the diff).

Run: python3 scripts/test_commit_msg_lint.py  (wired into make check).
Exit 0 = pass, 1 = fail.
"""
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "commit_msg_lint.py"


def _lint(msg: str) -> int:
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as tf:
        tf.write(msg)
        path = tf.name
    try:
        return subprocess.run(
            ["python3", str(SCRIPT), path],
            capture_output=True,
            text=True,
        ).returncode
    finally:
        Path(path).unlink(missing_ok=True)


def main() -> int:
    failures = []

    # COMPARISON in the body is blocked without a wordlist (tracked rule).
    if _lint("feat(x): do the thing\n\nThis surpasses the baseline.\n") == 0:
        failures.append("surpasses in body should block, did not")

    # A clean message passes.
    if _lint("feat(x): do the thing\n\nThe delegation stays inline.\n") != 0:
        failures.append("clean message was blocked")

    # Scissors line + commented diff below are stripped: the lint must NOT
    # scan the diff. A .rs comment line in the diff that contains
    # "surpasses" must not trip the commit gate. The scissors line matches
    # the real git commit -v format (dashes on both sides of >8).
    msg_with_diff = (
        "feat(x): do the thing\n"
        "\n"
        "The delegation stays inline.\n"
        "\n"
        "# ------------------------ >8 ------------------------\n"
        "diff --git a/f.rs b/f.rs\n"
        "-// this layout surpasses the older one\n"
        "+// the layout is inline\n"
    )
    if _lint(msg_with_diff) != 0:
        failures.append(
            "scissors-stripped message with a surpasses line in the diff "
            "was blocked - the diff below the scissors must be ignored"
        )

    # Subject with a codename is still blocked (the scissors strip does not
    # remove the subject).
    if _lint("feat(S26): do the thing\n\nbody\n") == 0:
        failures.append("codename S26 in subject should block, did not")

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        print(f"\n[commit-msg-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[commit-msg-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
