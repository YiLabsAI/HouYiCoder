#!/usr/bin/env python3
"""Regression test: a 60s-gate timeout must reap cargo's test-binary
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
  - the gated cargo invocation times out at GATE_SECS=1
  - the long-lived grandchild is dead once run_tests.py returns (killpg
    reaped the session); before the fix the grandchild survived
"""
import os
import subprocess
import sys
import tempfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
RUN_TESTS = REPO_ROOT / "scripts" / "run_tests.py"

FAKE_CARGO = """#!/usr/bin/env python3
# Stand-in for cargo: --no-run exits at once (the pre-compile step); the
# gated run spawns a long-lived grandchild (the orphan class) then sleeps
# past the gate.
import os, sys, subprocess, time
if "--no-run" in sys.argv:
    raise SystemExit(0)
g = subprocess.Popen(["sleep", "30"])
open(os.environ["HOYI_FAKE_PIDFILE"], "w").write(str(g.pid))
time.sleep(30)
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
        # A clean git repo: no crates/ changes -> run_tests takes the
        # plain-test path (cargo test --workspace --lib), which the fake
        # cargo turns into the grandchild-spawn shape.
        repo = tmp / "repo"
        repo.mkdir()
        for args in (
            ["git", "init", "-q"],
            ["git", "config", "user.email", "t@x"],
            ["git", "config", "user.name", "t"],
        ):
            subprocess.run(["git", "-C", str(repo), *args[1:]], check=True)
        subprocess.run(
            ["git", "-C", str(repo), "commit", "--allow-empty", "-m", "init", "-q"],
            check=True,
        )
        env = {
            **os.environ,
            "PATH": f"{tmp}:{os.environ['PATH']}",
            "GATE_SECS": "1",
            "HOYI_FAKE_PIDFILE": str(pidfile),
        }
        r = subprocess.run(
            [sys.executable, str(RUN_TESTS)],
            cwd=str(repo), env=env, capture_output=True, text=True,
        )
        if r.returncode != 1:
            failures.append(
                f"expected exit 1 (gate timeout), got {r.returncode}; "
                f"stderr={r.stderr!r}"
            )
        if "exceeded 1s gate" not in r.stderr:
            failures.append(f"missing timeout banner; stderr={r.stderr!r}")
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
