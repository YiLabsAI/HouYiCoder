#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "expected one benchmark: reward | tui | history | cache" >&2
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
    cache)
        # Live: one endpoint, one ratio. The model and the credential come from
        # the resolved settings, so point HOUYICODER_CONFIG_HOME at a settings
        # file to measure a model other than the default one; the run prints the
        # endpoint, the key origin, and the model it actually measured.
        run "$CARGO" test -p houyicoder-service --test cache_hit_rate \
            -- --ignored --nocapture
        ;;
    tui)
        run env HOUYI_GIT_SHA="$(git rev-parse --short HEAD)" \
            HOUYI_BENCH_RUN_ID="$(date +%Y-%m-%d-%H%M%S)-$(git rev-parse --short HEAD)" \
            "$CARGO" test -p houyicoder-tui --lib frame_timing_benchmark \
            --release -- --ignored --nocapture
        ;;
    history)
        # The PTY measurement drives the real binary, so build it first; the
        # test binary follows the same profile (houyi_binary_path switches on
        # debug_assertions). Serial threads keep the timing uncontended. One
        # run id across both halves so their sample dirs join up.
        HISTORY_SHA="$(git rev-parse --short HEAD)"
        HISTORY_RUN_ID="$(date +%Y-%m-%d-%H%M%S)-$HISTORY_SHA"
        run "$CARGO" build --release --bin houyi
        run env HOUYI_GIT_SHA="$HISTORY_SHA" HOUYI_BENCH_RUN_ID="$HISTORY_RUN_ID" \
            "$CARGO" test -p houyicoder-tui --test measure_history \
            --release -- --ignored --nocapture --test-threads 1
        run env HOUYI_GIT_SHA="$HISTORY_SHA" HOUYI_BENCH_RUN_ID="$HISTORY_RUN_ID" \
            "$CARGO" test -p houyicoder-cli --release --bin houyi \
            session::history::tests::test_scan_state_baseline \
            -- --ignored --nocapture
        ;;
    "")
        echo "benchmark name required: reward | tui | history | cache" >&2
        exit 2
        ;;
    *)
        echo "unknown benchmark: '$BENCHMARK' (expected: reward | tui | history | cache)" >&2
        exit 2
        ;;
esac
