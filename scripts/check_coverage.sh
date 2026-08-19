#!/usr/bin/env bash
# Coverage gate: fails the build when workspace UNIT-test line coverage drops
# below one global threshold. Uses cargo-llvm-cov. Skipped (warn) if the tool
# is not installed so the gate is opt-in until the environment has it -- CI
# installs it explicitly so the skip path cannot silently pass there.
#
# Install once:
#   rustup component add llvm-tools-preview
#   cargo install cargo-llvm-cov
#
# Unit-only (--lib): integration tests do not count toward this number. They
# validate end-to-end journeys, and journey completeness is reviewed as story
# coverage, not as a line percentage. Counting them here would let an
# end-to-end path stand in for unit tests a module still owes. Bin targets fall
# outside --lib for the same reason they fall outside the unit suite: they are
# wiring, exercised by the integration and PTY suites instead. Measured on this
# workspace the difference is small either way (integration tests move the
# total by well under a point), so including them would buy noise, not signal.
#
# One global threshold, not one per crate. A per-crate floor has to be pinned on
# the platform that pinned it, and this workspace has crates whose entire module
# set is target_os-gated -- the sandbox backends share no source file at all
# between macOS and Linux, so the same per-crate floor measures a disjoint code
# set on a different runner and then passes or fails for a reason that has
# nothing to do with the change under test. A workspace total dilutes any single
# platform-gated crate to well under a point, which is what makes one number
# meaningful on every runner.
#
# The trade-off is real and worth stating: one crate can regress several points
# without moving the workspace total. The NEW-code gate carries that load --
# check_diff_coverage.py holds every added or modified line to the same
# threshold, per change, which is where a regression actually enters.
#
# The lcov lands where check_diff_coverage.py looks for it, so running this gate
# leaves the diff gate nothing to recompile.

set -eo pipefail

if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
  echo "warn: cargo-llvm-cov not installed; coverage gate skipped." >&2
  echo "      install: rustup component add llvm-tools-preview && cargo install cargo-llvm-cov" >&2
  exit 0
fi

THRESHOLD=${COV_THRESHOLD:-85}

# Isolate the instrumented build cache (target/cov) so it does not thrash the
# plain dev cache. Clean profraw only (not the build): llvm-cov merges every
# sample it finds, and the instrumented binary is cargo-dep-tracked so only the
# samples go stale.
COV_DIR="target/cov"
LCOV="$COV_DIR/houyi-cov.lcov"
find "$COV_DIR" -name '*.profraw' -delete 2>/dev/null || true
CARGO_TARGET_DIR="$COV_DIR" cargo llvm-cov --no-cfg-coverage --lib --workspace \
  --lcov --output-path "$LCOV"

# Stale-mapping guard: the line table is baked into the instrumented binary, so
# a binary older than the last edit attributes every number to the wrong code.
# check() is the sole decider for all three outcomes (missing file / stale /
# clean) -- calling it unconditionally, not under [ -f ], is what keeps a report
# that produced no file from silently skipping the guard and letting the
# comparison below print a misleading percentage.
if ! python3 scripts/cov_lcov.py --check "$LCOV"; then
  rm -f "$LCOV"
  exit 1
fi

# Workspace total from the lcov's own per-file summaries: LF is lines found, LH
# is lines hit. Summing them needs no second cargo invocation.
read -r found hit < <(awk -F: '
  /^LF:/ {f += $2}
  /^LH:/ {h += $2}
  END    {print f+0, h+0}
' "$LCOV")

if [ "$found" -eq 0 ]; then
  echo "error: lcov reports zero executable lines; refusing to pass a vacuous gate." >&2
  exit 1
fi

pct=$(awk -v h="$hit" -v f="$found" 'BEGIN{printf "%.2f", 100*h/f}')
if awk -v p="$pct" -v t="$THRESHOLD" 'BEGIN{exit !(p+0 < t+0)}'; then
  echo "error: workspace unit line coverage ${pct}% < ${THRESHOLD}% threshold (${hit}/${found})" >&2
  exit 1
fi
echo "ok: workspace unit line coverage ${pct}% >= ${THRESHOLD}% (${hit}/${found})"
