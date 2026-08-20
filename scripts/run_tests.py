#!/usr/bin/env python3
"""Run the test suite with a wall-clock gate + terse (dot) output.

make check runs the UNIT suite (cargo test --lib) before every commit, so
the suite must stay fast; a slow test (a forgotten sleep, a real-network
call, or a per-test heavy lazy init like the BPE tokenizer) gates the repo.

Output is terse by default: libtest --format terse prints dots + a per-
binary summary, not one line per test (which floods the screen and pushes
earlier output out of scrollback). Set NEXTEST=1 to use cargo-nextest
instead (one line per test with per-test timing) when you need per-test
detail or want a single test crash to not abort the rest.

cargo test is the default over nextest on purpose: nextest isolates each
test in its own process, so a per-test lazy init (the tiktoken BPE table,
~300ms) is paid per test; cargo test shares the process, paying it once
per binary. For a suite with a heavy OnceLock that path is faster and the
output is the terse dot form.

Integration tests (tests/) are a separate, heavier suite: run `make
check-full` before a push. They are NOT in the make check gate (too slow
for the unit-gate budget); check-full uses a larger ceiling for the
integration binaries.

The unit-gate timeout is GATE_SECS; over CHECK_BUDGET_WARN is a warning
signal -- prune slow steps or raise CHECK_BUDGET_WARN. The gate covers test
EXECUTION only: the test binaries are compiled in a separate, ungated pass
first, so a cold build cannot trip a ceiling meant to catch a slow test.
"""
import os
import shutil
import subprocess
import sys

# FULL needs a higher default ceiling (heavier subprocess E2E), but both
# paths honor GATE_SECS so CI can tune the budget per step (ci.yml sets 120
# for the unit step, 300 for the integration step). The old code hardcoded
# 120 for FULL and ignored the env, making ci.yml's 300 a dead config.
FULL = "--full" in sys.argv
GATE_SECS = int(os.environ.get("GATE_SECS", "120" if FULL else "60"))


def changed_crates():
    """Crates with .rs changes vs HEAD (staged + unstaged), for scoping the
    coverage pass. Empty on a clean tree (no diff -> no cov needed). Bin-only
    crates (no src/lib.rs) are dropped: `cargo llvm-cov --lib --package` errors
    on a crate with no library target, so a change confined to a bin-only
    crate (e.g. the cli bin) would break the gate. Those crates' coverage is
    measured by the whole-workspace `make verify` pass instead."""
    try:
        out = subprocess.run(
            ["git", "diff", "--name-only", "HEAD", "--", "crates/"],
            capture_output=True, text=True, check=False,
        ).stdout
    except FileNotFoundError:
        return []
    names = set()
    for line in out.splitlines():
        parts = line.split("/")
        if len(parts) >= 2 and parts[0] == "crates":
            names.add(parts[1])
    # Drop bin-only crates: `cargo llvm-cov --lib --package` errors on a crate
    # with no library target. Those crates are covered by the whole-workspace
    # `make verify` pass instead of the commit gate.
    with_lib = [
        n for n in sorted(names)
        if os.path.exists(os.path.join("crates", n, "src", "lib.rs"))
    ]
    return with_lib


# Both paths use GATE_SECS (FULL-defaulted higher above). The gate covers
# test EXECUTION only. On the plain-cargo path the binaries are pre-built
# with --no-run below, so a cold compile does not count there; the coverage
# and nextest paths build inside the gate (see the pre-build block). A real
# slowdown still fails closed for investigation instead of hiding.
gate = GATE_SECS

use_nextest = os.environ.get("NEXTEST") == "1"
# When cargo-llvm-cov is installed, run the unit suite through it so the test
# run ALSO produces an lcov trace at LCOV_PATH. check_diff_coverage.py reuses
# that trace instead of compiling an instrumented build a second time (saves a
# cold compile per make check). Falls back to plain cargo test otherwise.
use_cov = shutil.which("cargo-llvm-cov") is not None and not use_nextest
# Isolate the instrumented build cache from the plain dev cache. cargo keys
# its build cache on RUSTFLAGS; llvm-cov runs with instrumentation flags,
# plain cargo test does not -- sharing the default target/ thrashes the
# cache (a full rebuild on every cov<->plain switch, even for a docs-only
# change). Pin the cov cache to target/cov/ so the plain target/ stays warm
# + both caches go incremental on real .rs changes only.
COV_TARGET_DIR = os.path.join("target", "cov")
LCOV_PATH = os.path.join(COV_TARGET_DIR, "houyi-cov.lcov")

# Scope the coverage pass to the CHANGED crates only (--package), not the
# whole workspace. cargo-llvm-cov reuses the cached instrumented binaries for
# unchanged crates (does not re-instrument or re-run them), so the typical
# 1-2-crate edit runs ~1.2s warm instead of ~3.2s for the whole workspace.
# The lcov carries only the changed crates' coverage -- all diff-cov needs,
# since changed lines live in changed crates. A cross-crate regression in an
# UNCHANGED dependent (a permission API change breaking tui) is not caught
# here; run `make check-full` (the whole workspace) before a push. Set
# FULL_COV=1 to force the whole-workspace coverage pass.
force_full_cov = os.environ.get("FULL_COV") == "1"
crates = [] if force_full_cov else changed_crates()


def drop_stale_profraw():
    """Delete raw coverage samples left by earlier runs.

    The instrumented run keeps its build cache on purpose, which is what makes
    the warm path fast, but the same flag also leaves the raw sample files in
    place and the report merges every one it finds. Samples from an earlier
    revision carry that revision's line numbers, so merging them attributes hits
    to whatever now sits at those lines.

    This is one of two staleness vectors. The other is the instrumented
    binary's line table, which is baked in at compile time and cannot be
    fixed by dropping samples alone: a binary built before the last edit
    describes the file as it used to be. When that is the cause, clearing
    profraw is not enough -- the whole cov cache must go (rm -rf target/cov)
    so the next run rebuilds the table. The stale-mapping guard in
    scripts/cov_lcov.py catches this by checking for lines past the end of
    the source file, which only a stale table produces.

    Only the samples go here. The compiled artifacts stay, so this costs
    nothing but the re-run that was going to happen anyway.
    """
    if not os.path.isdir(COV_TARGET_DIR):
        return
    for root, _dirs, files in os.walk(COV_TARGET_DIR):
        for name in files:
            if name.endswith(".profraw"):
                try:
                    os.remove(os.path.join(root, name))
                except OSError:
                    pass


cmd = ["cargo"]
# True when the gated command is a plain cargo test, the only shape whose
# compile step can be lifted out of the gate by a --no-run pass. The
# instrumented and nextest builds manage their own build lifecycle.
plain_test = False
if use_nextest:
    cmd += ["nextest", "run", "--workspace"]
    if not FULL:
        cmd.append("--lib")
elif use_cov:
    if FULL or force_full_cov:
        # Full gate (or forced): whole workspace, full lcov trace.
        drop_stale_profraw()
        cmd += ["llvm-cov", "--no-clean", "--workspace", "--lcov", "--output-path",
                LCOV_PATH]
        if not FULL:
            cmd.append("--lib")
        cmd += ["--", "--format", "terse"]
    elif crates:
        # Changed crates only: reuse cached binaries for the unchanged crates
        # (no re-instrument, no run for them). The lcov carries only the
        # changed crates' coverage -- all diff-cov needs.
        drop_stale_profraw()
        cmd += ["llvm-cov", "--lib", "--no-clean"]
        for c in crates:
            cmd += ["--package", c]
        cmd += ["--lcov", "--output-path", LCOV_PATH, "--", "--format", "terse"]
    else:
        # Clean tree (no .rs diff vs HEAD): no new lines for diff-cov to gate,
        # so skip the instrumented build + run plain cargo test (no coverage
        # overhead). diff-cov reports "nothing new to gate".
        cmd += ["test", "--workspace", "--lib", "--", "--format", "terse"]
        plain_test = True
else:
    cmd += ["test", "--workspace"]
    if not FULL:
        cmd.append("--lib")
    cmd += ["--", "--format", "terse"]
    plain_test = True

# HOUYICODER_FAST_TOKENS: the unit suite does not assert on token counts
# (it asserts turns/outcomes), so skip the ~300ms tiktoken BPE load and
# use a char-based estimate. Tokenizer accuracy tests opt out via
# Tokenizer::real(). Production never sets this.
#
# HOUYICODER_SANDBOX_NO_ENFORCE: skip the KERNEL fence while still building
# the session. Landlock restricts the calling process irreversibly, so one
# test constructing a sandbox fences the whole test binary and every sibling
# temp-dir write then fails with EACCES. Production never sets this.
env = {
    **os.environ,
    "HOUYICODER_FAST_TOKENS": "1",
    "HOUYICODER_SANDBOX_NO_ENFORCE": "1",
}
# Route the instrumented build to the isolated cov cache so it does not
# displace the plain dev cache (see COV_TARGET_DIR above). Plain
# cargo test (the clean-tree branch) keeps the default target/.
if use_cov:
    env["CARGO_TARGET_DIR"] = COV_TARGET_DIR

# Compile the test binaries BEFORE starting the clock, so the gate measures
# test execution and not the compiler. The gate exists to catch a slow test,
# but the timeout wrapped the whole cargo invocation, so a cold build counted
# against it. The build is cold on every CI run: the preceding cargo check
# emits the check profile (no codegen, no link), and rust-cache keeps only
# third-party deps -- the workspace's own test binaries are always rebuilt.
# Locally that costs ~22s versus ~9s of actual test time; on Windows the MSVC
# link is slow enough to blow a 120s gate outright. The same env is used for
# both invocations so the gated run is a guaranteed cache hit. Left to their
# own build lifecycle: nextest, and the coverage passes (instrumented build).
if plain_test:
    pre_cmd = ["cargo", "test", "--workspace", "--no-run"]
    if not FULL:
        pre_cmd.append("--lib")
    subprocess.run(pre_cmd, env=env, check=False)

try:
    # Merge stderr into stdout (cargo prints "Running unittests ..." and
    # "Finished" to stderr; interleaving keeps real order). Then filter out
    # the per-binary "Running unittests" / "running N tests" headers and
    # blank lines (noise); keep dots, test result, Finished, and failures.
    # cargo test has no flag to suppress the per-binary headers.
    result = subprocess.run(
        cmd, timeout=gate, env=env,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )
    # Aggregate per-binary "test result" lines into ONE summary line (Python/go
    # terse style). Drop dots, per-binary headers, Running/Finished/info lines.
    # On failure, print the failure detail (kept in stdout after "failures:").
    import re
    passed = failed = ignored = 0
    failures = []
    in_failures = False
    res_re = re.compile(r"test result: \w+\.\s+(\d+) passed;\s+(\d+) failed;\s+(\d+) ignored")
    for line in result.stdout.splitlines():
        s = line.strip()
        m = res_re.search(s)
        if m:
            passed += int(m.group(1))
            failed += int(m.group(2))
            ignored += int(m.group(3))
            in_failures = False
            continue
        if s == "failures:":
            in_failures = True
            continue
        if in_failures and s and not s.startswith("----"):
            failures.append(s)
    status = "ok" if failed == 0 else "FAILED"
    scope = "changed crates only; full suite runs at push" if not FULL else "full workspace"
    print(f"test result: {status}. {passed} passed; {failed} failed; {ignored} ignored ({scope})")
    for f in failures:
        print(f)
    sys.exit(result.returncode)
except subprocess.TimeoutExpired:
    print(f"error: tests exceeded {gate}s gate", file=sys.stderr)
    print("       (a slow test blocks every commit; mock external calls,", file=sys.stderr)
    print("        or a per-test lazy init is paid per process under nextest)", file=sys.stderr)
    print(f"       (running {'full' if FULL else 'unit'} suite)", file=sys.stderr)
    sys.exit(1)
