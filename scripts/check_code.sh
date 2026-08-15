#!/usr/bin/env bash
# Code quality gate — fail-fast. Rust-mapped, fail-fast.
# Stops at the FIRST failure so the error output stays visible.

set -euo pipefail

GREEN='\033[0;32m'; RED='\033[0;31m'; YELLOW='\033[1;33m'; NC='\033[0m'
STARTED_AT=$(python3 -c 'import time; print(time.perf_counter())' 2>/dev/null || echo 0)
STEP_TIMINGS=()

run_check() {
	local name=$1; shift
	echo -e "${YELLOW}▶ Running ${name}...${NC}"
	local t0; t0=$(python3 -c 'import time; print(time.perf_counter())' 2>/dev/null || echo 0)
	if "$@"; then
		local t1; t1=$(python3 -c 'import time; print(time.perf_counter())' 2>/dev/null || echo 0)
		STEP_TIMINGS+=("${name}:$(python3 -c "print(f'{$t1 - $t0:.2f}')" 2>/dev/null || echo "?")")
		echo -e "${GREEN}✓ ${name} passed${NC}"
		echo ""
	else
		echo ""
		echo -e "${RED}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
		echo -e "${RED}✗ ${name} FAILED — stopping here.${NC}"
		echo -e "${RED}  Fix the errors above, then re-run: make check${NC}"
		echo -e "${RED}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
		exit 1
	fi
}

print_summary() {
	local total; total=$(python3 -c "print(f'{$(python3 -c 'import time; print(time.perf_counter())' 2>/dev/null || echo 0) - ${STARTED_AT}:.2f}')" 2>/dev/null || echo "?")
	echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	echo "check timing summary"
	for step in "${STEP_TIMINGS[@]+"${STEP_TIMINGS[@]}"}"; do
		echo "  - ${step} s"
	done
	echo "  - total:${total} s"
	# Quality feedback (not a gate): warn when the total drifts past the
	# budget. A cold first-compile (no target/ cache) legitimately exceeds
	# the warm budget, so it uses a higher threshold. Tunable via
	# CHECK_BUDGET_WARM (default 30) / CHECK_BUDGET_COLD (default 120).
	local warm=${CHECK_BUDGET_WARM:-30}
	local cold=${CHECK_BUDGET_COLD:-120}
	local mode=warm budget=$warm
	if [ ! -d target ]; then mode=cold; budget=$cold; fi
	if [ "$total" != "?" ]; then
		if python3 -c "exit(0 if float('$total') <= $budget else 1)" 2>/dev/null; then
			:
		else
			echo -e "${YELLOW}⚠ total ${total}s over ${budget}s ${mode} budget — prune slow steps, or raise CHECK_BUDGET_WARM/COLD${NC}"
		fi
	fi
	echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
}
trap print_summary EXIT

echo "🔍 Running code quality checks..."
echo ""

CARGO="${CARGO:-cargo}"
# Tracked + untracked (non-ignored) .rs files, so new files are gated too —
# git ls-files alone misses untracked files (a CJK comment in a new file
# slipped through once).
# git ls-files alone misses untracked files (a CJK comment in a new file
# slipped through once).
RS_FILES="$(git ls-files '*.rs' 2>/dev/null; git ls-files --others --exclude-standard '*.rs' 2>/dev/null)"

# CHECK_SKIP_RUST: CI runs fmt/clippy/test as separate steps already;
# check_code.sh would duplicate them. Set this to run ONLY the Python
# gates (naming, no-cjk, ratchets, judgment-refs, etc.) without the
# Rust steps. The gate list stays in ONE place (this file); CI does not
# duplicate it.
SKIP_RUST="${CHECK_SKIP_RUST:-0}"
if [ "$SKIP_RUST" = "1" ]; then
  SKIP_STEPS="fmt-check clippy test diff-cov"
else
  SKIP_STEPS=""
fi

# Helper: skip a step if it's in SKIP_STEPS.
_should_skip() {
  case " $SKIP_STEPS " in
    *" $1 "*) return 0 ;;
    *) return 1 ;;
  esac
}
run_rust_check() {
  _should_skip "$1" && { echo -e "${YELLOW}▶ Skipping $1 (CHECK_SKIP_RUST)\n${NC}"; return 0; }
  shift
  run_check "$@"
}

run_rust_check "fmt-check"  "fmt-check"   "$CARGO" fmt --all -- --check
run_rust_check "clippy"     "clippy"      "$CARGO" clippy --workspace --all-targets --no-deps -- -D warnings
run_check "comments"    python3 scripts/check_rs_comments.py $RS_FILES
if [ -f scripts/check_product_refs.py ]; then
  run_check "product-refs" python3 scripts/check_product_refs.py
fi
run_check "no-cjk"      python3 scripts/check_no_cjk.py
run_check "script-tests"  bash -c 'python3 scripts/test_hook_rust.py && python3 scripts/test_cov_lcov.py && python3 scripts/test_flat_prefix.py && python3 scripts/test_diff_cov.py && python3 scripts/test_stderr_gate.py && python3 scripts/test_comment_block_length.py && python3 scripts/test_app_coupling.py && python3 scripts/test_file_size.py && python3 scripts/test_struct_fields.py && python3 scripts/test_app_mut_borrows.py && python3 scripts/test_module_dead_code.py'
run_check "rust-naming" python3 scripts/check_rust_naming.py $RS_FILES
run_check "struct-fields" python3 scripts/check_struct_fields.py
run_check "app-mut-borrows" python3 scripts/check_app_mut_borrows.py
run_check "dead-code-ratchet" python3 scripts/check_module_dead_code.py
run_check "comment-block-length" python3 scripts/check_comment_block_length.py
if [ -f scripts/check_judgment_refs.py ]; then
  run_check "judgment-refs" python3 scripts/check_judgment_refs.py
fi
if [ -f scripts/check_sdd_naming.py ]; then
  run_check "sdd-naming" python3 scripts/check_sdd_naming.py
fi
run_check "file-size"    python3 scripts/check_file_size.py $RS_FILES
run_check "dep-graph"    python3 scripts/check_dep_graph.py
run_check "stderr"       python3 scripts/check_stderr.py
run_check "gatectx-field-names"  python3 scripts/check_gatectx_field_names.py
run_rust_check "test"       "test"        python3 scripts/run_tests.py
run_rust_check "diff-cov"   "diff-cov"    python3 scripts/check_diff_coverage.py

# Report-only: stale test names in docs (pre-existing backlog; will
# become blocking once the backlog is cleared + the regex is tightened
# to avoid false positives like "test_data" in prose).
if [ -f scripts/check_acceptance_anchors.py ]; then
  echo -e "${YELLOW}▶ Checking acceptance anchors (report-only)...${NC}"
  python3 scripts/check_acceptance_anchors.py || echo -e "${YELLOW}  (report-only — stale anchors exist, not blocking)${NC}"
fi

# Report-only: journey coverage (commands in code with no journey row) +
# doc-code sync (acceptance rows whose status contradicts their note).
# Both have pre-existing drift; wired report-only so the drift is visible
# in every make-check run, becoming blocking once cleared.
if [ -f scripts/check_journey_coverage.py ]; then
  echo -e "${YELLOW}▶ Checking journey coverage (report-only)...${NC}"
  python3 scripts/check_journey_coverage.py || echo -e "${YELLOW}  (report-only — journey gaps exist, not blocking)${NC}"
fi
if [ -f scripts/check_doc_sync.py ]; then
  echo -e "${YELLOW}▶ Checking doc-code sync (report-only)...${NC}"
  python3 scripts/check_doc_sync.py || echo -e "${YELLOW}  (report-only — status-vs-note drift exists, not blocking)${NC}"
fi

echo -e "${GREEN}✓ All checks passed! Ready to commit.${NC}"
