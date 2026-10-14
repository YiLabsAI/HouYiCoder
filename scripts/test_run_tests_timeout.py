#!/usr/bin/env python3
"""Regression test: a gate timeout must reap cargo's test-binary
grandchildren, not orphan them under init.

run_tests.py launches `cargo test` under start_new_session so cargo is its
own session leader; on timeout it kills the whole session group. The bug it
fixes: subprocess.run(timeout=...) killed only cargo, leaving test binaries
reparented to init - they kept a PTY + the houyi binary alive, accumulated
across timed-out gates, and starved the next run of CPU/PTYs (a death spiral
that read as "verify hangs"). This test reproduces the spawn shape (a
grandchild that outlives the gate) and asserts the grandchild is gone after
run_tests.py exits.

Covered:
  - the gated cargo invocation times out at the gate budget below
  - the long-lived grandchild is dead once run_tests.py returns (killpg
    reaped the session); before the fix the grandchild survived

The gate budget is a fraction of a second: the timeout path only needs the
gate to fire while the stand-in cargo is still running, and waiting a whole
second for that made this the slowest step in the gate by far.
"""
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
RUN_TESTS = REPO_ROOT / "scripts" / "run_tests.py"

# The gate must fire after the stand-in cargo has spawned its grandchild
# (a few ms for a shell script) and before it finishes sleeping, so the
# pidfile exists to assert on. A tenth of a second leaves that margin many
# times over while keeping this step around 0.2s, most of which is the two
# interpreter starts plus the deliberate wait.
GATE_SECS = 0.1

# Stand-in for cargo, in shell so the two invocations do not each pay a
# Python interpreter start. --no-run exits at once (the pre-compile step);
# the gated run spawns a grandchild that outlives everything (the orphan
# class) and then holds the gate's pipe for a short while, the way cargo
# does while it runs tests.
#
# The two sleeps have different jobs. The grandchild must still be alive
# when the gate has returned, or the liveness check below could only ever
# see a process that exited on its own. The foreground sleep must be short
# enough that a gate which fails to reap the tree still returns, so the
# failure is reported as a surviving orphan rather than as a hang.
FAKE_CARGO = """#!/bin/sh
case "$*" in
  *--no-run*) exit 0 ;;
esac
sleep 600 >/dev/null 2>&1 &
echo $! > "$HOYI_FAKE_PIDFILE"
sleep 5
"""


def main() -> int:
    failures = []
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        # Fake cargo on PATH ahead of the real one.
        fake_bin = tmp / "cargo"
        fake_bin.write_text(FAKE_CARGO)
        fake_bin.chmod(0o755)
        pidfile = tmp / "grandchild.pid"
        # The run directory is deliberately not a git repo: changed_crates()
        # then finds no crates/ diff and run_tests takes the plain-test path
        # (cargo test --workspace --lib), which the fake cargo turns into the
        # grandchild-spawn shape. Creating a repo here would only slow it down.
        workdir = tmp / "work"
        workdir.mkdir()
        env = {
            **os.environ,
            "PATH": f"{tmp}:{os.environ['PATH']}",
            "GATE_SECS": str(GATE_SECS),
            "HOYI_FAKE_PIDFILE": str(pidfile),
        }
        started = time.monotonic()
        r = subprocess.run(
            [sys.executable, str(RUN_TESTS)],
            cwd=str(workdir), env=env, capture_output=True, text=True,
        )
        elapsed = time.monotonic() - started
        if r.returncode != 1:
            failures.append(
                f"expected exit 1 (gate timeout), got {r.returncode}; "
                f"stderr={r.stderr!r}"
            )
        if f"exceeded {GATE_SECS:g}s gate" not in r.stderr:
            failures.append(f"missing timeout banner; stderr={r.stderr!r}")
        # A gate that fired must return right after it fired. The stand-in
        # cargo holds the gate's pipe for five seconds, so a gate that killed
        # only the leader returns after that wait; a reaping gate returns in
        # well under a second.
        if elapsed > 3:
            failures.append(
                f"run_tests returned after {elapsed:.1f}s for a "
                f"{GATE_SECS:g}s gate - it waited on something it did not reap"
            )
        if not pidfile.exists():
            failures.append(
                "grandchild pidfile never written - the gated cargo invocation "
                "did not reach the spawn shape (test setup is wrong)"
            )
        else:
            pid = int(pidfile.read_text().strip())
            try:
                os.kill(pid, 0)
                failures.append(
                    f"grandchild {pid} still alive after run_tests exited - "
                    "the timeout orphaned it (killpg did not reap the session)"
                )
                # Best-effort cleanup so the test process does not linger.
                try:
                    os.kill(pid, 9)
                except ProcessLookupError:
                    pass
            except ProcessLookupError:
                pass  # reaped: the fix held.

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        print(f"\n[run-tests-timeout-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[run-tests-timeout-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
