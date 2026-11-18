#!/bin/sh
# Full review, Phase 2 (area rust-unit): runs the characterisation tests of the pure
# logic RUNS times (default 3) in release and writes one JSON result in the format
# of nuvira-pro docs/review/manual-tests.md ("nuvira-review-result/1").
#
#   tools/review-p2-rust-unit-json.sh MT-ID [RUNS]
#
# Environment: NUVIRA_MACHINE (default hostname), NUVIRA_CONFIG_IDS, NUVIRA_OUT
# (default ~/nuvira-review/results), CARGO_TARGET_DIR. On macOS NUVIRA_KEY_STORE
# defaults to "file" (no Keychain dialogs).
set -u
id=${1:?usage: review-p2-rust-unit-json.sh MT-ID [RUNS]}
runs=${2:-3}
cd "$(dirname "$0")/.." || exit 1
machine=${NUVIRA_MACHINE:-$(hostname -s)}
out=${NUVIRA_OUT:-$HOME/nuvira-review/results}
mkdir -p "$out"
day=$(date -u +%Y%m%d)
base="$out/$id-$machine-$day"
log="$base.log"
[ "$(uname)" = Darwin ] && export NUVIRA_KEY_STORE=${NUVIRA_KEY_STORE-file}
: >"$log"

suites="nuvira-proto|--test framing_characterisation
nuvira-transport|--test fec_characterisation
nuvira-transport|--test streams_characterisation
nuvira-transport|--test signal_characterisation
nuvira-transport|--lib p2p::characterisation
nuvira-update|--test manifest_characterisation
nuvira-secret|--test secret_characterisation
nuvira-agent|--lib characterisation"

start=$(date +%s)
build=0
printf '%s\n' "$suites" | while IFS='|' read -r pkg args; do
    # shellcheck disable=SC2086
    cargo test --release -p "$pkg" $args --no-run >>"$log" 2>&1 || echo fail >"$base.buildfail"
done
[ -e "$base.buildfail" ] && build=1 && rm -f "$base.buildfail"
build_s=$(($(date +%s) - start))

entries=""
passed=0
failed=0
exit_code=0
i=1
start=$(date +%s)
while [ "$i" -le "$runs" ] && [ "$build" -eq 0 ]; do
    for line in $(printf '%s\n' "$suites" | tr ' ' '#'); do
        pkg=${line%%|*}
        args=$(printf '%s' "${line#*|}" | tr '#' ' ')
        echo "=== run $i: $pkg $args" >>"$log"
        # shellcheck disable=SC2086
        res=$(cargo test --release -p "$pkg" $args 2>&1)
        code=$?
        printf '%s\n' "$res" >>"$log"
        r=$(printf '%s\n' "$res" | grep '^test result:' | tail -1)
        p=$(printf '%s' "$r" | sed -n 's/.* \([0-9]*\) passed.*/\1/p')
        f=$(printf '%s' "$r" | sed -n 's/.* \([0-9]*\) failed.*/\1/p')
        passed=$((passed + ${p:-0}))
        failed=$((failed + ${f:-0}))
        [ "$code" -ne 0 ] && exit_code=$code
        entries="$entries{\"name\": \"$pkg $args (run $i)\", \"kind\": \"characterisation\", \"completed\": $([ -n "$r" ] && echo true || echo false), \"passed\": ${p:-0}, \"failed\": ${f:-0}, \"ignored\": 0},"
    done
    i=$((i + 1))
done
test_s=$(($(date +%s) - start))
[ "$build" -ne 0 ] && exit_code=-1
sha=$(git rev-parse HEAD 2>/dev/null || echo unknown)
cat >"$base.json" <<EOF
{
  "schema": "nuvira-review-result/1",
  "id": "$id",
  "kind": "rust-test-suite",
  "machine": "$machine",
  "config_ids": "${NUVIRA_CONFIG_IDS:-}",
  "date": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
  "code": {"repo": "nuvira", "ref": "review/p2-rust-unit", "sha": "$sha"},
  "os": {"name": "$(uname -s) $(uname -r)", "kernel": "$(uname -sr) $(uname -m)"},
  "toolchain": {"rustc": "$(rustc --version)", "cargo": "$(cargo --version)"},
  "env": {"NUVIRA_KEY_STORE": "${NUVIRA_KEY_STORE:-}", "profile": "release"},
  "command": "tools/review-p2-rust-unit-json.sh $id $runs",
  "build_exit_code": $build,
  "exit_code": $exit_code,
  "build_seconds": $build_s,
  "test_seconds": $test_s,
  "log": "$(basename "$log")",
  "summary": {"binaries": $((runs * 8)), "passed": $passed, "failed": $failed, "ignored": 0},
  "binaries": [${entries%,}],
  "notes": "Phase 2 rust-unit characterisation tests, $runs runs"
}
EOF
echo "written $base.json (passed $passed, failed $failed)"
