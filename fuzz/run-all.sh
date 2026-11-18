#!/bin/sh
# Runs every fuzz target for SECONDS (default 600) with debug assertions on
# (overflow checks), two at a time, and prints for each: execs, corpus size,
# crash artifacts. Logs in fuzz/logs/<target>.log, crashes in fuzz/artifacts/.
# Needs a nightly toolchain and cargo-fuzz on PATH.
set -u
seconds=${1:-600}
cd "$(dirname "$0")" || exit 1
mkdir -p logs
cargo fuzz build -O -a || exit 1
run() {
    t=$1
    mkdir -p "corpus/$t"
    cargo fuzz run -O -a "$t" -- -max_total_time="$seconds" -rss_limit_mb=2048 -timeout=10 \
        -print_final_stats=1 >"logs/$t.log" 2>&1
    echo "$t exit=$? $(grep -E '^stat::number_of_executed_units' "logs/$t.log" | tail -1)" \
        "corpus=$(ls "corpus/$t" | wc -l) crashes=$(ls "artifacts/$t" 2>/dev/null | wc -l)"
}
set -- $(cargo fuzz list)
while [ $# -gt 0 ]; do
    run "$1" &
    [ $# -gt 1 ] && run "$2" &
    wait
    shift; [ $# -gt 0 ] && shift
done
