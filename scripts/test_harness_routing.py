#!/usr/bin/env python3
"""Regression tests for test-suite and benchmark command routing."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def invoke(command: list[str]) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env["HOUYI_DRY_RUN"] = "1"
    env["CARGO"] = "cargo-under-test"
    return subprocess.run(
        command,
        cwd=ROOT,
        env=env,
        text=True,
        capture_output=True,
        check=False,
        timeout=10,
    )


def run(script: str, *args: str) -> subprocess.CompletedProcess[str]:
    return invoke([str(ROOT / "scripts" / script), *args])


def run_make(*args: str) -> subprocess.CompletedProcess[str]:
    return invoke(["make", *args])


def main() -> int:
    failures: list[str] = []

    ui = run("suite.sh", "ui")
    if ui.returncode or "build --bin houyi" not in ui.stdout or r"binary\(/\^ui_/\)" not in ui.stdout:
        failures.append(f"ui suite route incorrect: {ui.stdout!r} {ui.stderr!r}")

    sandbox = run("suite.sh", "sandbox")
    if sandbox.returncode or "houyicoder-sandbox --lib" not in sandbox.stdout:
        failures.append(f"sandbox suite route incorrect: {sandbox.stdout!r}")

    live = run("suite.sh", "live")
    if (
        live.returncode
        or r"binary\(/live_agent/\)" not in live.stdout
        or r"binary\(/openai_compat_real/\)" not in live.stdout
        or r"binary\(/mcp_live_server/\)" not in live.stdout
    ):
        failures.append(f"live suite route incorrect: {live.stdout!r} {live.stderr!r}")
    if "reward_bench" in live.stdout:
        failures.append("live suite must not include benchmarks")

    reward = run("benchmark.sh", "reward")
    lines = [line for line in reward.stdout.splitlines() if line.startswith("RUN")]
    if reward.returncode or len(lines) != 2:
        failures.append(f"reward benchmark must run two legs: {reward.stdout!r}")
    elif (
        "-u HOUYICODER_REWARD_OFF" not in lines[0]
        or "test_reward_on_pair" not in lines[0]
        or "HOUYICODER_REWARD_OFF=1" not in lines[1]
        or "test_reward_off_pair" not in lines[1]
    ):
        failures.append(f"reward ON/OFF routing incorrect: {lines!r}")

    migrated = run("test.sh", "ui")
    if migrated.returncode != 2 or "make suite ui" not in migrated.stderr:
        failures.append("legacy test scope must provide a suite migration hint")

    for script, selectors, marker in (
        ("test.sh", ("unit", "integration"), "expected one test scope"),
        ("suite.sh", ("live", "ui"), "expected one suite"),
        ("benchmark.sh", ("reward", "reward"), "expected one benchmark"),
    ):
        multiple = run(script, *selectors)
        if multiple.returncode != 2 or marker not in multiple.stderr:
            failures.append(f"{script} must reject multiple selectors")

    make_test = run_make("test", "unit")
    if make_test.returncode or "cargo-under-test test --workspace --lib --bins" not in make_test.stdout:
        failures.append(f"two-word test target is broken: {make_test.stdout!r}")
    make_ui = run_make("suite", "ui")
    if make_ui.returncode or "binary\\(/\\^ui_/\\)" not in make_ui.stdout:
        failures.append(f"two-word suite target is broken: {make_ui.stdout!r}")
    make_reward = run_make("benchmark", "reward")
    if make_reward.returncode or "test_reward_off_pair" not in make_reward.stdout:
        failures.append("two-word benchmark target is broken")

    for selector, qualified in (
        ("unit", "make test unit"),
        ("ui", "make suite ui"),
        ("sandbox", "make suite sandbox"),
        ("live", "make suite live"),
        ("reward", "make benchmark reward"),
    ):
        bare = run_make(selector)
        if bare.returncode != 2 or qualified not in bare.stderr:
            failures.append(f"bare {selector} must provide qualified usage")

    for goals in (
        ("test", "unit", "integration"),
        ("suite", "live", "ui"),
        ("benchmark", "reward", "reward"),
    ):
        multiple = run_make(*goals)
        if multiple.returncode != 2:
            failures.append(f"make must reject multiple selectors: {goals!r}")

    test_script = (ROOT / "scripts" / "test.sh").read_text()
    if ". ./.env" in test_script:
        failures.append("correctness tests must not source live credentials")

    provider_test = (
        ROOT / "crates" / "houyicoder-provider" / "tests" / "openai_compat_real.rs"
    ).read_text()
    if "#[ignore" not in provider_test:
        failures.append("real provider test must remain ignored outside the live suite")

    makefile = (ROOT / "Makefile").read_text()
    if "NEXTEST_IGNORED_REPORT" in makefile or "Running real-infra" in makefile:
        failures.append("verify must not retain report-only live or benchmark routing")
    verify_filter = next(
        (line for line in makefile.splitlines() if line.startswith("NEXTEST_VERIFY_FILTER")),
        "",
    )
    for binary in ("live_agent", "openai_compat_real", "mcp_live_server", "reward_bench"):
        if f"not(binary(/{binary}/))" not in verify_filter:
            failures.append(f"verify must exclude {binary}")

    if failures:
        for failure in failures:
            print(f"FAIL: {failure}", file=sys.stderr)
        print(f"\n[test-harness-tests] {len(failures)} failure(s).", file=sys.stderr)
        return 1
    print("[test-harness-tests] ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
