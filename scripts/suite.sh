#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "expected one suite: ui | sandbox | live" >&2
    exit 2
fi
SUITE="$1"
CARGO="${CARGO:-cargo}"

run() {
    if [ "${HOUYI_DRY_RUN:-0}" = "1" ]; then
        printf 'RUN'
        printf ' %q' "$@"
        printf '\n'
    else
        "$@"
    fi
}

ensure_nextest() {
    if [ "${HOUYI_DRY_RUN:-0}" = "1" ]; then
        printf 'REQUIRE nextest\n'
    else
        ./scripts/ensure_nextest.sh
    fi
}

case "$SUITE" in
    ui)
        ensure_nextest
        run "$CARGO" build --bin houyi
        run "$CARGO" nextest run --run-ignored only -p houyicoder-tui -E 'binary(/^ui_/)' -j 3
        ;;
    sandbox)
        ensure_nextest
        run "$CARGO" nextest run --run-ignored only -p houyicoder-sandbox --lib
        ;;
    live)
        if [ -f .env ] && [ "${HOUYI_DRY_RUN:-0}" != "1" ]; then
            set -a
            . ./.env
            set +a
        fi
        ensure_nextest
        run "$CARGO" nextest run --workspace --run-ignored only -E 'binary(/live_agent/) or binary(/openai_compat_real/) or binary(/mcp_live_server/)'
        ;;
    "")
        echo "suite name required: ui | sandbox | live" >&2
        exit 2
        ;;
    *)
        echo "unknown suite: '$SUITE' (expected: ui | sandbox | live)" >&2
        exit 2
        ;;
esac
