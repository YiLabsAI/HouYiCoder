#!/usr/bin/env bash
# Coverage gate: fails the build when line coverage drops below a per-crate
# threshold. Uses cargo-llvm-cov. Skipped (warn) if the tool is not installed
# so the gate is opt-in until the environment has it.
#
# Install once:
#   rustup component add llvm-tools-preview
#   cargo install cargo-llvm-cov
#
# Runs one workspace --lib coverage pass, then aggregates the per-file report
# by crate (the report has no per-crate rows, only per-file + TOTAL) and
# compares each crate's line coverage to its threshold.
#
# Threshold rationale: thresholds are pinned to each crate's current
# coverage floor (a ratchet, not a fixed target -- see threshold_for below),
# so the gate is green at today's level and fails on any regression. Raise a
# number only when coverage improves. Override all with COV_THRESHOLD=<n>.
#
# Bash 3.2 compatible (no associative arrays — macOS /bin/bash is 3.2).

set -eo pipefail

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
  echo "warn: cargo-llvm-cov not installed; coverage gate skipped." >&2
  echo "      install: rustup component add llvm-tools-preview && cargo install cargo-llvm-cov" >&2
  exit 0
fi

DEFAULT=${COV_THRESHOLD:-80}

# Per-crate line-coverage threshold (case fallback to DEFAULT).
#
# Ratchet, not a fixed target: each crate's threshold is pinned to its
# CURRENT coverage floor -- the gate is green at today's level, and any
# regression fails. Raise a threshold only when coverage actually improves
# (commit the new floor in the same commit that raised it), so the number
# only ever goes up. A green gate that is never run is the same as no gate,
# so this runs from `make check-full` (pre-push), not `make check`
# (the full-workspace instrumented run does not fit the unit-gate budget).
#
# Tiny-crate exemption: houyicoder-async has ~11 executable lines, where an
# 85% threshold is one line of statistical noise -- a config error, not a
# coverage gap. It is exempt (0) rather than accommodated by a global
# threshold drop, which would also mask the genuinely-under-covered crates.
threshold_for() {
  case "$1" in
    houyicoder-core)       echo 86 ;;
    houyicoder-protocol)   echo 90 ;;
    houyicoder-context)   echo 90 ;;
    houyicoder-config)    echo 90 ;;
    houyicoder-permission) echo 90 ;;
    houyicoder-provider)  echo 85 ;;
    houyicoder-async)     echo 0  ;;
    houyicoder-resilience) echo 85 ;;
    houyicoder-memory)    echo 85 ;;
    houyicoder-session)   echo 85 ;;
    houyicoder-sandbox)   echo 80 ;;
    houyicoder-api)       echo 71 ;;
    houyicoder-service)   echo 75 ;;
    houyicoder-tui)       echo 70 ;;
    houyicoder-cli)       echo 0  ;;
    *)                    echo "$DEFAULT" ;;
  esac
}

# One instrumented nextest run for the whole workspace: runs unit + integration
# tests WITH coverage in one parallel pass + writes the lcov inline. Isolate
# the cov build cache (target/cov) so it does not thrash the plain dev cache.
COV_DIR="target/cov"
# Clean profraw only (not the build): llvm-cov merges every sample it finds,
# and the instrumented binary is cargo-dep-tracked so only the samples go stale.
find "$COV_DIR" -name '*.profraw' -delete 2>/dev/null || true
LCOV_TMP="$COV_DIR/per-crate.lcov"
CARGO_TARGET_DIR="$COV_DIR" cargo llvm-cov nextest --no-cfg-coverage --workspace --lcov --output-path "$LCOV_TMP"
# Stale-mapping guard: the line table is baked into the instrumented binary,
# so a binary older than the last edit attributes every number to the wrong
# code. check() is the sole decider for all three outcomes (missing file /
# stale / clean) -- calling it unconditionally, not under [ -f ], is what
# keeps a report that produced no file from silently skipping the guard and
# letting the threshold loop below print a misleading percentage.
if ! python3 scripts/cov_lcov.py --check "$LCOV_TMP"; then
  rm -f "$LCOV_TMP"
  exit 1
fi
rm -f "$LCOV_TMP"

# Aggregate per-file line coverage by crate, then compare to thresholds.
status=0
while IFS=' ' read -r c total missed; do
  [ -n "$c" ] || continue
  th=$(threshold_for "$c")
  pct=$(awk -v t="$total" -v m="$missed" 'BEGIN{if(t+0==0){print 0}else{printf "%.1f", 100*(t-m)/t}}')
  if awk -v p="$pct" -v t="$th" 'BEGIN{exit !(p+0 < t+0)}'; then
    echo "error: $c line coverage ${pct}% < ${th}% threshold (${total}-${missed}/${total})" >&2
    status=1
  else
    echo "ok: $c ${pct}% >= ${th}%"
  fi
done < <(CARGO_TARGET_DIR="$COV_DIR" cargo llvm-cov report --summary-only 2>/dev/null | awk '
  /^TOTAL/ || /^File/ || /^---/ {next}
  /\/src\// {
    path=$1; sub(/\/src\/.*/,"",path); sub(/.*\//,"",path)
    total=$8; missed=$9
    if (total+0>0) { t[path]+=total; m[path]+=missed }
  }
  END { for (c in t) printf "%s %d %d\n", c, t[c], m[c] }
')

exit $status
