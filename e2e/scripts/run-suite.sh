#!/usr/bin/env bash
# The whole e2e suite (default + long, every case 5 times) on this machine, with
# a nuvira-review-result/1 file for docs/review/manual-results/. Used for the
# manual entries (macOS: also the real-desktop file cases, which capture the
# real screen: only on a test machine whose screen may be used, and after
# granting Screen Recording to the terminal).
#
#   e2e/scripts/run-suite.sh MACHINE [ID] [--real-desktop]
#
# Needs cargo and go (or NUVIRA_BENCH_SERVER_EXE). Results in
# ~/nuvira-review/results/<ID>-<MACHINE>-<yyyymmdd>.json (+ .log).
set -euo pipefail
machine="${1:?machine name}"
id="${2:-MT-E2E-SUITE}"
cd "$(dirname "$0")/.."
out="$HOME/nuvira-review/results"
mkdir -p "$out"
base="$out/$id-$machine-$(date -u +%Y%m%d)"
export NUVIRA_E2E_LONG=1 NUVIRA_E2E_REPEAT="${NUVIRA_E2E_REPEAT:-5}"
export NUVIRA_E2E_RESULT_JSON="$base.json" NUVIRA_E2E_RESULT_ID="$id" NUVIRA_E2E_MACHINE="$machine"
export NUVIRA_E2E_OUT="$base-run"
if [ "${3:-}" = "--real-desktop" ]; then export NUVIRA_E2E_REAL_DESKTOP=1; fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/nuvira-review/target-e2e}"
cargo test --release --test e2e >"$base.log" 2>&1 && code=0 || code=$?
echo "exit $code; results in $base.json"
exit "$code"
