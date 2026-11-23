#!/usr/bin/env python3
"""Regression test: a gate timeout must reap cargo's test-binary
grandchildren, not orphan them under init or block on pipe holders.

run_tests.py launches `cargo test` under start_new_session so cargo is its
own session leader; on timeout it kills the whole session group. The bug it
fixes: subprocess.run(timeout=...) killed only cargo, leaving test binaries
reparented to init - they kept a PTY + the houyi binary alive, accumulated
across timed-out gates, and starved the next run of CPU/PTYs (a death spiral
that read as "verify hangs"). This test reproduces both failure shapes:
  - a detached grandchild (stdout/stderr redirected to /dev/null) that stays
    alive as an orphan if only the leader dies
  - a pipe-holding child (inherits stdout/stderr) that blocks communicate()
    until it exits if only the leader dies

The test runs with a minimal isolated environment (no leaked FULL_COV,
NEXTEST, HOUYICODER_COV_DIR, or external cargo-llvm-cov/sccache) and asserts
kernel-level process death plus a generous hang guard rather than racing
total Python/git startup wall-clock time against a short threshold.
"""
import os
import signal
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
RUN_TESTS = REPO_ROOT / "scripts" / "run_tests.py"

# Long enough for /bin/sh to spawn both stand-in children and write both
# pidfiles even under heavy host load, while keeping the normal run ~0.3s.
GATE_SECS = 0.25

# Hard hang guard on the outer subprocess. Both stand-in children live for
# 600s, so a broken killpg blocks communicate() until this guard trips, while
# a healthy run returns right after GATE_SECS regardless of startup jitter.
HANG_GUARD_SECS = 10

# Stand-in for cargo: --no-run exits at once (pre-compile pass); the gated
# run spawns both a detached orphan candidate (closed stdio) and a foreground
# pipe holder (inherits stdout/stderr), then waits on the pipe holder.
FAKE_CARGO = """#!/bin/sh
case "$*" in
  *--no-run*) exit 0 ;;
esac
sleep 600 >/dev/null 2>&1 &
echo $! > "$HOYI_FAKE_PIDFILE"
sleep 600 &
pipe_pid=$!
echo "$pipe_pid" > "$HOYI_FAKE_PIPE_PIDFILE"
wait "$pipe_pid"
"""


def _read_pid(path: Path) -> int | None:
    if not path.exists():
        return None
    text = path.read_text().strip()
    return int(text) if text else None


def _is_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def _force_reap(path: Path) -> None:
    pid = _read_pid(path)
    if pid is None:
        return
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def main() -> int:
    failures = []
    with tempfile.TemporaryDirectory() as tmp_dir:
        tmp = Path(tmp_dir)
        fake_bin = tmp / "cargo"
        fake_bin.write_text(FAKE_CARGO)
        fake_bin.chmod(0o755)
        pidfile = tmp / "grandchild.pid"
        pipe_pidfile = tmp / "pipe_holder.pid"
        workdir = tmp / "work"
        workdir.mkdir()
        # Minimal isolated environment: no external cargo-llvm-cov, sccache,
        # NEXTEST, FULL_COV, or host COV/TARGET overrides can alter the path
        # or touch real worktree artifacts. TMPDIR stays inside tmp so any
        # runtime scratch directory run_tests creates is cleaned up on exit.
        env = {
            "PATH": f"{tmp}:/usr/bin:/bin",
            "TMPDIR": str(tmp),
            "GATE_SECS": str(GATE_SECS),
            "HOYI_FAKE_PIDFILE": str(pidfile),
            "HOYI_FAKE_PIPE_PIDFILE": str(pipe_pidfile),
        }
        try:
            try:
                r = subprocess.run(
                    [sys.executable, str(RUN_TESTS)],
                    cwd=str(workdir),
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=HANG_GUARD_SECS,
                )
            except subprocess.TimeoutExpired as exc:
                failures.append(
                    f"run_tests hung past {HANG_GUARD_SECS}s for a "
                    f"{GATE_SECS:g}s gate (pipe-holding child was not reaped); "
                    f"stderr={exc.stderr!r}"
                )
                r = None

            if r is not None:
                if r.returncode != 1:
                    failures.append(
                        f"expected exit 1 (gate timeout), got {r.returncode}; "
                        f"stderr={r.stderr!r}"
                    )
                if f"exceeded {GATE_SECS:g}s gate" not in r.stderr:
                    failures.append(f"missing timeout banner; stderr={r.stderr!r}")

            for label, path in (
                ("detached grandchild", pidfile),
                ("pipe-holding child", pipe_pidfile),
            ):
                pid = _read_pid(path)
                if pid is None:
                    failures.append(
                        f"{label} pidfile never written - gated cargo did not "
                        "reach the spawn shape"
                    )
                elif _is_alive(pid):
                    failures.append(
                        f"{label} {pid} still alive after run_tests exited - "
                        "timeout orphaned it (killpg did not reap the session)"
                    )
        finally:
            _force_reap(pidfile)
            _force_reap(pipe_pidfile)

    if failures:
        for f in failures:
            print(f"FAIL: {f}", file=sys.stderr)
        print(f"\n[run-tests-timeout-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[run-tests-timeout-tests] ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
