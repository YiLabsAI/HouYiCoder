# Fail on any pipeline command that exits non-zero, not just the last.
# Without this, piping make output masks failures behind a green exit code.
SHELL := /bin/bash -o pipefail

.PHONY: help install setup-hooks \
        format fmt-check lint typecheck \
        test test-cov \
        unit integration ui sandbox all \
        quick-check check check-full \
        check-deps check-stderr \
        deny clean


CARGO ?= cargo

help:
	@echo "houyicoder development commands"
	@echo "=============================="
	@echo ""
	@echo "Setup:"
	@echo "  make install          Install stable Rust toolchain + rustfmt + clippy"
	@echo ""
	@echo "Development:"
	@echo "  make quick-check      Fast checks (fmt-check + clippy, no tests)"
	@echo "  make check            Full pre-commit gate (fmt + clippy + comments + naming +"
	@echo "                        file-size + dep-graph + stderr + unit tests + diff-cov)"
	@echo "  make check-full       Pre-push: check + integration tests + coverage gate"
	@echo "  make format           Auto-format (cargo fmt)"
	@echo "  make fmt-check        Verify formatting (--check)"
	@echo "  make lint             Clippy with -D warnings"
	@echo "  make typecheck        cargo check (Rust type check)"
	@echo ""
	@echo "Testing (one umbrella, category as an arg):"
	@echo "  make test             All tests: unit + integration + ui (default = all)"
	@echo "  make test unit        Inline unit tests (--lib --bins)"
	@echo "  make test integration Integration tests (tests/ binaries; live self-skip w/o .env)"
	@echo "  make test ui          PTY UI tests: build the houyi bin + run tests/ui_*.rs --ignored"
	@echo "  make test sandbox     Live sandbox tests: real sandbox-exec, needs macOS, #[ignore]"
	@echo "  make test-cov         Coverage gates (workspace unit total + diff-cov)"
	@echo "  (NEXTEST=1 selects cargo-nextest for unit/integration legs)"
	@echo ""
	@echo "  make check-deps       Layering dep-graph assertion (binding; exits 1 on runtime-dep violation)"
	@echo "  make check-stderr     Console-write assertion (binding; print macros must not reach the TUI screen)"
	@echo ""
	@echo "Other:"
	@echo "  make deny             cargo-deny (license/advisory) if installed"
	@echo "  make clean            cargo clean"
install:
	rustup toolchain install stable
	rustup component add rustfmt clippy rust-src

setup-nextest:
	@./scripts/ensure_nextest.sh

setup-hooks:
	@hooks_dir=$$(git rev-parse --git-common-dir)/hooks; \
	printf '#!/usr/bin/env bash\nset -euo pipefail\nrepo=$$(git rev-parse --show-toplevel 2>/dev/null || pwd)\n[ -f "$$repo/scripts/pre_commit_hook.sh" ] || exit 0\nexec bash "$$repo/scripts/pre_commit_hook.sh"\n' > "$$hooks_dir/pre-commit"; \
	chmod +x "$$hooks_dir/pre-commit"; \
	printf '#!/usr/bin/env bash\nset -euo pipefail\nrepo=$$(git rev-parse --show-toplevel 2>/dev/null || pwd)\n[ -f "$$repo/scripts/pre_push_hook.sh" ] || exit 0\nexec bash "$$repo/scripts/pre_push_hook.sh"\n' > "$$hooks_dir/pre-push"; \
	chmod +x "$$hooks_dir/pre-push"; \
	chmod +x scripts/pre_push_hook.sh scripts/pre_commit_hook.sh
	@echo "pre-commit hook installed (file-type-narrowed make check; no-op if absent)."
	@echo "pre-push hook installed (coverage gate, skips make check; no-op if absent)."
	@echo "Remove: rm $$(git rev-parse --git-common-dir)/hooks/pre-commit $$(git rev-parse --git-common-dir)/hooks/pre-push"

format:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

typecheck:
	$(CARGO) check --workspace --all-targets

# Test umbrella: one entry point, category is a positional arg.
#   make test            = all (unit + integration + ui)
#   make test unit|integration|ui|sandbox|all  = that category
# Category names are also targets so make test ui does not error; they
# re-invoke test with the arg via MAKECMDGOALS. scripts/test.sh dispatches.
# make check does NOT depend on make test -- the commit gate stays run_tests.py.
test:
	@./scripts/test.sh $(filter-out $@,$(MAKECMDGOALS))

# Category markers: make test <cat> and make <cat> both work.
# The no-op recipe silences the "Nothing to be done" message.
unit integration ui sandbox all: test
	@:

test-cov:
	@./scripts/check_coverage.sh
	@python3 scripts/check_diff_coverage.py

quick-check:
	@./scripts/quick_check.sh

check:
	@./scripts/check_code.sh

# Full gate: make check + the workspace coverage total. One instrumented
# unit-suite pass, whose lcov the diff gate in make check reuses rather than
# compiling a second instrumented build. Integration tests are deliberately
# outside the number: they validate end-to-end journeys, which are reviewed as
# story coverage rather than as a line percentage. Run before a merge or push.
check-full: check
	@./scripts/check_coverage.sh

# Verify gate: check-full + the ignored test suite (sandbox-exec, PTY UI,
# live-in-name unit tests) run in parallel via nextest + doc-stale detection.
# cargo test runs ignored tests serially (~18min); nextest -j=N finishes in
# seconds. The ignored suite runs in parallel here so a green gate is fast
# enough to run before every commit, not just CI.
#
# Filter split: real-infra tests (live_agent + live MCP server, need API key
# or network) and pinned bug_repro (expected to fail) run report-only below.
# reward_bench is a real-LLM benchmark excluded from verify entirely:
#   cargo test -p houyicoder-service --test reward_bench -- --ignored
NEXTEST_IGNORED_BLOCKING := -E 'not(test(/bug_repro/)) and not(binary(/live_agent/)) and not(test(/live_mcp_real_server/)) and not(binary(/reward_bench/))'
NEXTEST_IGNORED_REPORT := -E 'test(/bug_repro/) or binary(/live_agent/) or test(/live_mcp_real_server/) or binary(/reward_bench/)'
# Parallel-safety: fresh_temp_dir retries on AlreadyExists (nextest gives each
# test its own process, so the per-process SEQ counter restarts at 0; an
# OS-recycled pid could mint a path matching a leftover dir). No --retries
# needed; a consistent failure still surfaces. -j 8 caps concurrent
# houyi-binary spawns (each PTY test spawns the bin) -- -j 16 raced
# intermittent wait_for timeouts on loaded hosts.
verify: check-full
	@./scripts/ensure_nextest.sh
	@echo "▶ Building the houyi bin (the PTY tests spawn it via a hardcoded path;"
	@echo "  cargo test does not build the plain bin target, only the test binaries)."
	@$(CARGO) build --bin houyi
	@echo "▶ Running ignored suite in PARALLEL (BLOCKING: real failures fail verify)."
	@echo "  Real-infra (live_agent binary + live_mcp_real_server, need API key /"
	@echo "  network) + pinned bug_repro are excluded here + run report-only below."
	@start=$$(date +%s); \
	$(CARGO) nextest run --workspace --run-ignored only -j 8 $(NEXTEST_IGNORED_BLOCKING); status=$$?; \
	end=$$(date +%s); total=$$((end - start)); \
	warn_budget=${VERIFY_BUDGET_WARN:-60}; \
	if [ $$total -gt $$warn_budget ]; then \
		printf "\033[1;33m⚠ verify ignored-suite %ds over %ds budget — prune slow tests (sleep→events, shared fixtures) or raise VERIFY_BUDGET_WARN\033[0m\n" $$total $$warn_budget; \
	fi; \
	exit $$status
	@echo "▶ Running real-infra + pinned bug-repro tests (report-only):"
	@$(CARGO) nextest run --workspace --run-ignored only $(NEXTEST_IGNORED_REPORT) || true
	@echo "▶ Running doc-stale detection..."
	@if [ -f scripts/check_doc_stale.py ]; then python3 scripts/check_doc_stale.py || true; fi
	@echo "▶ Running structure-facts report (for deep review)..."
	@python3 scripts/report_structure_facts.py || true
	@echo "✓ verify passed: unit + integration + coverage + ignored-suite(blocking,parallel) + live/bug-repro(report) + docs(report)"

# Layering dependency-graph assertion. Joins the blocking check gate once
# the full migration completes.
check-deps:
	@python3 scripts/check_dep_graph.py

# Console-write assertion: a print macro under the alternate screen lands in
# the input box (where the cursor sits). Runs inside make check; this target
# is for reading the remaining migration stock on its own.
check-stderr:
	@python3 scripts/check_stderr.py

## review: L3 structure facts + the review checklist (no tests, ~2s).
## Run before claiming done: the facts are on the table, the questions
## are in the output. Judgment is yours.
review:
	@echo "▶ Structure Facts (L3 report-only)"
	@python3 scripts/report_structure_facts.py
	@echo ""
	@echo "Review the facts against these questions (judgment is yours):"
	@echo "  1. allow/expect: designed byproduct (shrinking) or substitute (growing)?"
	@echo "  2. new trait/helper: designed-first (>=2 call sites) or post-hoc extraction?"
	@echo "  3. cross-file duplication: enum match sites + clone top pairs checked?"
	@echo "  4. expect(reason=) reasons match the code? (false reason = hidden debt)"
	@echo "  5. struct fields: single concern-domain or mixed?"
	@echo "  6. naming: new words = new concepts or synonyms? describe what, not who-uses."
	@echo "  7. test files touched: single behavior-domain, name predicts contents?"
	@echo "Then the risk axes: correctness / safety / concurrency / compat /"
	@echo "robustness / observability / perf / operability / cognitive load / tests."

deny:
	@if command -v cargo-deny >/dev/null 2>&1; then \
		$(CARGO) deny check; \
	else \
		echo "cargo-deny not installed — run: cargo install cargo-deny"; \
	fi

	clean:
	$(CARGO) clean
