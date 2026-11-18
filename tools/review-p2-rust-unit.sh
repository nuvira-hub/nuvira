#!/bin/sh
# Runs the characterisation tests of the pure logic (full review, Phase 2,
# area rust-unit) N times in debug and release, and prints one line per run:
#   <profile> <run> <test binary> passed=<n> failed=<n>
# Usage: tools/review-p2-rust-unit.sh [runs] (default 3). Uses CARGO_TARGET_DIR as set.
set -u
runs=${1:-3}
cd "$(dirname "$0")/.." || exit 1

suites() {
    echo "-p nuvira-proto --test framing_characterisation"
    echo "-p nuvira-transport --test fec_characterisation"
    echo "-p nuvira-transport --test streams_characterisation"
    echo "-p nuvira-transport --test signal_characterisation"
    echo "-p nuvira-transport --lib p2p::characterisation"
    echo "-p nuvira-update --test manifest_characterisation"
    echo "-p nuvira-secret --test secret_characterisation"
    echo "-p nuvira-agent --lib characterisation"
}

status=0
for profile in debug release; do
    flag=""
    [ "$profile" = release ] && flag="--release"
    # Build first, so that the timings below are of the tests only.
    suites | while read -r args; do
        # shellcheck disable=SC2086
        cargo test $flag $args --no-run -q >/dev/null 2>&1 || echo "$profile build FAILED: $args"
    done
    i=1
    while [ "$i" -le "$runs" ]; do
        suites | while read -r args; do
            start=$(date +%s)
            # shellcheck disable=SC2086
            out=$(cargo test $flag $args 2>&1)
            code=$?
            line=$(printf '%s\n' "$out" | grep '^test result:' | tail -1)
            passed=$(printf '%s' "$line" | sed -n 's/.* \([0-9]*\) passed.*/\1/p')
            failed=$(printf '%s' "$line" | sed -n 's/.* \([0-9]*\) failed.*/\1/p')
            echo "$profile $i [$args] passed=${passed:-?} failed=${failed:-?} exit=$code seconds=$(($(date +%s) - start))"
            if [ "$code" -ne 0 ]; then
                printf '%s\n' "$out" | grep -E 'panicked|FAILED|failures:' | head -20
            fi
        done
        i=$((i + 1))
    done
done
exit $status
