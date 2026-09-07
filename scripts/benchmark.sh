#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "expected one benchmark: reward" >&2
    exit 2
fi
BENCHMARK="$1"
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

case "$BENCHMARK" in
    reward)
        if [ -f .env ] && [ "${HOUYI_DRY_RUN:-0}" != "1" ]; then
            set -a
            . ./.env
            set +a
        fi
        run env -u HOUYICODER_REWARD_OFF "$CARGO" test -p houyicoder-service \
            --test reward_bench test_reward_on_pair -- --ignored --nocapture
        run env HOUYICODER_REWARD_OFF=1 "$CARGO" test -p houyicoder-service \
            --test reward_bench test_reward_off_pair -- --ignored --nocapture
        ;;
    "")
        echo "benchmark name required: reward" >&2
        exit 2
        ;;
    *)
        echo "unknown benchmark: '$BENCHMARK' (expected: reward)" >&2
        exit 2
        ;;
esac
