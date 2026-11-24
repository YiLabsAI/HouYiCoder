#!/usr/bin/env bash
# Correctness-test dispatcher. Selects test scope only; capability-dependent
# suites and benchmarks use their own dispatchers.
set -euo pipefail

# A test that reads or writes the settings file would otherwise reach the
# user config. Give the run an empty config home unless the caller set one,
# and remove it on exit. The dry-run path only prints commands, so it needs
# no directory and stays side-effect free.
if [ "${HOUYI_DRY_RUN:-0}" != "1" ] && [ -z "${HOUYICODER_CONFIG_HOME:-}" ]; then
    HOUYICODER_CONFIG_HOME="$(mktemp -d)"
    export HOUYICODER_CONFIG_HOME
    trap 'rm -rf "$HOUYICODER_CONFIG_HOME"' EXIT
fi

if [ "$#" -gt 1 ]; then
    echo "expected one test scope, got: $*" >&2
    exit 2
fi
SCOPE="${1:-all}"
CARGO="${CARGO:-cargo}"

run() {
    if [ "${HOUYI_DRY_RUN:-0}" = "1" ]; then
        printf 'RUN'
        printf ' %q' "$CARGO" test "$@"
        printf '\n'
    elif [ "${NEXTEST:-0}" = "1" ] && command -v cargo-nextest >/dev/null 2>&1; then
        "$CARGO" nextest run "$@"
    else
        "$CARGO" test "$@"
    fi
}

case "$SCOPE" in
    unit)
        run --workspace --lib --bins
        ;;
    integration)
        run --tests --workspace
        ;;
    all)
        run --workspace
        ;;
    ui|sandbox|live)
        echo "'$SCOPE' is a capability suite; run: make suite $SCOPE" >&2
        exit 2
        ;;
    *)
        echo "unknown test scope: '$SCOPE' (expected: unit | integration | all)" >&2
        exit 2
        ;;
esac
