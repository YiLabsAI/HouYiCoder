#!/usr/bin/env bash
# Test dispatcher — one entry point for the `make test [category]` umbrella.
# Categories:
#   unit         cargo test --workspace --lib --bins (inline unit tests)
#   integration  cargo test --tests --workspace (the tests/ binaries; live
#                tests self-skip without .env)
#   ui           the PTY UI tests: build the houyi bin + run every tests/ui_*
#                binary --ignored (they are #[ignore] so they never run in
#                `make check`/`make test`). One binary per UI category
#                (ui_permissions / ui_mode / ui_consent ...) sharing common/mod.rs.
#   sandbox      live sandbox tests: real sandbox-exec (macOS only), the
#                #[ignore] tests in houyicoder-sandbox that prove the fence
#                actually holds. Never run by the other categories.
#   all          unit + integration + ui (the default; bare `make test` = all)
# `NEXTEST=1` selects cargo-nextest for the unit/integration legs when installed.
set -euo pipefail

CAT="${1:-all}"

run() {
    # $@ = cargo args after `test`/`nextest run`.
    if [ "${NEXTEST:-0}" = "1" ] && command -v cargo-nextest >/dev/null 2>&1; then
        cargo nextest run "$@"
    else
        cargo test "$@"
    fi
}

run_ignored() {
    # Run #[ignore] tests in parallel via nextest (--run-ignored only).
    # nextest is required for verify + the manual `make test` legs (one
    # compiled binary, parallel subprocess spawns); cargo test runs them
    # serially (minutes vs seconds). $@ = nextest args. ensure_nextest.sh
    # is called by each ignored leg before this so nextest is present.
    cargo nextest run --run-ignored only "$@"
}

case "$CAT" in
    unit)
        run --workspace --lib --bins
        ;;
    integration)
        if [ -f .env ]; then set -a; . ./.env; set +a; fi
        run --tests --workspace
        ;;
    ui)
        ./scripts/ensure_nextest.sh
        cargo build --bin houyi
        # One parallel nextest run across every tests/ui_*.rs binary. A new
        # category file just works — the binary(/^ui_/) filter auto-includes
        # it. Replaces the old per-binary serial `cargo test -- --ignored`
        # loop (each spawn + wait_for was serial; nextest parallel cuts the
        # wall clock from minutes to seconds).
        run_ignored -p houyicoder-tui -E 'binary(/^ui_/)'
        ;;
    sandbox)
        # Live sandbox tests: real sandbox-exec (macOS). They are #[ignore]
        # in the houyicoder-sandbox lib so no other category picks them up;
        # this is the one entry that runs them. The fence-extension effect
        # test (live_added_dir_accessible) lives here. nextest runs them in
        # parallel via --run-ignored only (the old comment that nextest did
        # not accept --ignored was outdated — it takes --run-ignored only).
        ./scripts/ensure_nextest.sh
        run_ignored -p houyicoder-sandbox --lib
        ;;
    all|"")
        # Unit + integration run together via --workspace (non-ignored); the UI
        # tests are #[ignore], so run them explicitly via nextest in parallel.
        if [ -f .env ]; then set -a; . ./.env; set +a; fi
        run --workspace
        ./scripts/ensure_nextest.sh
        cargo build --bin houyi
        run_ignored -p houyicoder-tui -E 'binary(/^ui_/)'
        ;;
    *)
        echo "unknown test category: '$CAT' (expected: unit | integration | ui | sandbox | all)" >&2
        exit 1
        ;;
esac
