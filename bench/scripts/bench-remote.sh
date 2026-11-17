#!/usr/bin/env bash
# Runs the A/B rounds on a remote Windows PC (PC-LABORATORIO) with binaries built here
# (plan section 9.2 "--remote"). Normally started by bench-compare.sh --remote; it can
# also be run by hand after a bench-compare.sh build (--skip-build reuses $WORK/dist).
#
#   bench/scripts/bench-remote.sh --remote USER@HOST --dist DIR --out DIR [options]
#
# Options: --remote-dir 'C:\Users\me\nuvira-bench' (default C:\Users\<user>\nuvira-bench),
#   --rounds R (5), --tier quick|full, --suites micro,macro,res,long,go,go-soak, --filter RE,
#   --ids LIST, --idle S (10), --go-count N (2), --go-benchtime D (500ms), --machine ALIAS,
#   --config-ids LIST, --key FILE (default $NUVIRA_SSH_KEY or ~/.ssh/nuvira_test),
#   --mode task|ssh (default task), --perf (power plan "High performance" for the rounds,
#   the previous plan restored at the end, also on failure or interruption; recorded in the
#   results as run.machine.power.plan and .setup).
#
# Mode "task" (default) starts every run through a scheduled task in the interactive
# session of the logged-in user (/it), like tools/lan-test.sh: cases that need the
# desktop (screen capture B-MICRO-17, input B-MACRO-08b, windows) run there for real.
# Mode "ssh" runs them in the SSH session (no desktop: those cases report not-runnable).
#
# Results land in DIR/<side>/r<k>/ exactly as with a local run; the Go text outputs are
# converted here with the B binary's `import-go`. Exit 0, or 2 when a run did not finish.

set -u
set -o pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_SRC="$(cd "$SCRIPT_DIR/.." && pwd)"
. "$SCRIPT_DIR/lib.sh"

REMOTE=""; REMOTE_DIR=""; DIST=""; OUT=""; ROUNDS=5; TIER=quick; SUITES="micro,macro,res,go"
FILTER=""; IDS=""; IDLE=10; GO_COUNT=2; GO_BENCHTIME=500ms; MACHINE=""; CONFIG_IDS=""
KEY="${NUVIRA_SSH_KEY:-$HOME/.ssh/nuvira_test}"; MODE=task; PERF=0

die() { echo "bench-remote: $*" >&2; exit 2; }
say() { echo "== $*" >&2; }

while [ $# -gt 0 ]; do
    case "$1" in
        --remote) REMOTE="$2"; shift ;;
        --remote-dir) REMOTE_DIR="$2"; shift ;;
        --dist) DIST="$2"; shift ;;
        --out) OUT="$2"; shift ;;
        --rounds) ROUNDS="$2"; shift ;;
        --tier) TIER="$2"; shift ;;
        --suites) SUITES="$2"; shift ;;
        --filter) FILTER="$2"; shift ;;
        --ids) IDS="$2"; shift ;;
        --idle) IDLE="$2"; shift ;;
        --go-count) GO_COUNT="$2"; shift ;;
        --go-benchtime) GO_BENCHTIME="$2"; shift ;;
        --machine) MACHINE="$2"; shift ;;
        --config-ids) CONFIG_IDS="$2"; shift ;;
        --key) KEY="$2"; shift ;;
        --mode) MODE="$2"; shift ;;
        --perf) PERF=1 ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $1" ;;
    esac
    shift
done
[ -n "$REMOTE" ] && [ -n "$DIST" ] && [ -n "$OUT" ] || die "--remote, --dist and --out are required"
[ -n "$REMOTE_DIR" ] || REMOTE_DIR="C:\\Users\\${REMOTE%%@*}\\nuvira-bench"
[ -n "$MACHINE" ] || MACHINE="${REMOTE#*@}"
case "$MODE" in task|ssh) ;; *) die "--mode task or ssh" ;; esac

SSH="ssh -i $KEY -o BatchMode=yes -o ConnectTimeout=10 -o LogLevel=ERROR $REMOTE"
SCP="scp -q -i $KEY -o BatchMode=yes -o LogLevel=ERROR"
RD_SCP="/$(echo "$REMOTE_DIR" | tr '\\' '/')"
SUITES=" $(echo "$SUITES" | tr ',' ' ') "
SEL_IDS="$(echo "$IDS" | tr ',' ' ')"
RUST_SUITES=""
for s in micro macro res long; do has_word "$SUITES" "$s" && RUST_SUITES="${RUST_SUITES:+$RUST_SUITES,}$s"; done
JUDGE="$DIST/B/nuvira-bench.exe"
[ -x "$JUDGE" ] || die "missing $JUDGE"

rsh() { $SSH "$@"; }

# --perf: Windows power plan "High performance" (GUID of the built-in scheme) for the rounds.
HIGH_PERF=8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c
OLD_PLAN=""
POWER_SETUP=""
restore_plan() {
    [ -n "$OLD_PLAN" ] || return 0
    say "restoring power plan $OLD_PLAN on $REMOTE"
    rsh "powercfg /setactive $OLD_PLAN" >/dev/null 2>&1 \
        || echo "bench-remote: WARNING: could not restore power plan $OLD_PLAN on $REMOTE" >&2
    OLD_PLAN=""
}
if [ "$PERF" = 1 ]; then
    OLD_PLAN="$(rsh "powercfg /getactivescheme" 2>/dev/null | grep -o '[0-9a-f]\{8\}-[0-9a-f-]\{27\}' | head -n 1)"
    [ -n "$OLD_PLAN" ] || die "--perf: cannot read the active power plan on $REMOTE"
    trap 'restore_plan' EXIT
    trap 'exit 130' INT TERM
    rsh "powercfg /setactive $HIGH_PERF" >/dev/null 2>&1 || die "--perf: cannot activate High performance on $REMOTE"
    POWER_SETUP="power plan High performance $HIGH_PERF (was $OLD_PLAN, restored afterwards)"
    say "$POWER_SETUP"
fi

say "copying binaries to $REMOTE:$REMOTE_DIR"
rsh "if not exist \"$REMOTE_DIR\\A\" mkdir \"$REMOTE_DIR\\A\"" >/dev/null
rsh "if not exist \"$REMOTE_DIR\\B\" mkdir \"$REMOTE_DIR\\B\"" >/dev/null
rsh "if not exist \"$REMOTE_DIR\\tmp\" mkdir \"$REMOTE_DIR\\tmp\"" >/dev/null
rsh "if exist \"$REMOTE_DIR\\out\" rmdir /s /q \"$REMOTE_DIR\\out\"" >/dev/null
for side in A B; do
    for f in "$DIST/$side"/*.exe; do
        [ -f "$f" ] || continue
        $SCP "$f" "$REMOTE:$RD_SCP/$side/$(basename "$f")" || die "copy of $f failed"
    done
done

# One .cmd per run, written here and copied (CRLF line ends).
write_cmd() {
    # write_cmd SIDE ROUND FILE
    local side="$1" k="$2" file="$3" o="out\\$1\\r$2" pkg re
    {
        printf '@echo off\r\n'
        printf 'cd /d "%s"\r\n' "$REMOTE_DIR"
        printf 'set TMP=%s\\tmp\r\nset TEMP=%s\\tmp\r\n' "$REMOTE_DIR" "$REMOTE_DIR"
        printf 'set NUVIRA_BENCH_SIDE=%s\r\n' "$side"
        [ -n "$POWER_SETUP" ] && printf 'set NUVIRA_BENCH_POWER_SETUP=%s\r\n' "$POWER_SETUP"
        for v in NUVIRA_BENCH_MACRO_SECS NUVIRA_BENCH_LONG_SECS NUVIRA_BENCH_EVENTS NUVIRA_BENCH_NOISE_CORES; do
            eval "x=\${$v:-}"; [ -n "$x" ] && printf 'set %s=%s\r\n' "$v" "$x"
        done
        printf 'set NUVIRA_BENCH_SERVER_EXE=%s\\%s\\nuvira-server.exe\r\n' "$REMOTE_DIR" "$side"
        printf 'set NUVIRA_BENCH_AGENT_EXE=%s\\%s\\nuvira-agent.exe\r\n' "$REMOTE_DIR" "$side"
        printf 'set NUVIRA_BENCH_VIEWER_EXE=%s\\%s\\nuvira-viewer.exe\r\n' "$REMOTE_DIR" "$side"
        printf 'set NUVIRA_BENCH_ANIMATE_EXE=%s\\%s\\animate.exe\r\n' "$REMOTE_DIR" "$side"
        if [ "$MODE" = task ]; then
            # Desktop session of the signed-in user (PC-LABORATORIO): the cases that read
            # the real screen (B-MICRO-17, B-MACRO-08b, B-MACRO-10, B-RES-03) and move the
            # real mouse (B-MACRO-08b) are allowed here. Never set on the NUC.
            printf 'set NUVIRA_BENCH_REAL_CAPTURE=1\r\nset NUVIRA_BENCH_INPUT=1\r\nset NUVIRA_BENCH_SCREEN=1\r\n'
            # Motion for B-MICRO-17 when the animation was built (--with-binaries).
            if [ -f "$DIST/$side/animate.exe" ]; then
                printf 'set NUVIRA_BENCH_ANIMATE=%s\\%s\\animate.exe\r\n' "$REMOTE_DIR" "$side"
            fi
        fi
        printf 'if not exist %s mkdir %s\r\n' "$o" "$o"
        if [ -n "$RUST_SUITES" ]; then
            printf '%s\\nuvira-bench.exe run %s --tier %s --side %s --round %s --out out --inputs inputs --tmp tmp --machine "%s" --no-verify' \
                "$side" "$RUST_SUITES" "$TIER" "$side" "$k" "$MACHINE"
            [ -n "$CONFIG_IDS" ] && printf ' --config-ids %s' "$CONFIG_IDS"
            [ -n "$FILTER" ] && printf ' --filter "%s"' "$FILTER"
            printf ' > %s\\run.log 2>&1\r\n' "$o"
        fi
        if has_word "$SUITES" go; then
            for pkg in relay signal store; do
                re="$(go_bench_regex "$pkg")" || continue
                [ -n "$re" ] || continue
                printf '%s\\%s.test.exe -test.run "^$" -test.bench "%s" -test.benchmem -test.count %s -test.benchtime %s -test.timeout 60m > %s\\go-%s.txt 2>&1\r\n' \
                    "$side" "$pkg" "$re" "$GO_COUNT" "$GO_BENCHTIME" "$o" "$pkg"
            done
        fi
        if has_word "$SUITES" go-soak && [ "$k" = 1 ]; then
            secs=300; [ "$TIER" = full ] && secs=1800
            printf 'set NUVIRA_BENCH_SOAK=1\r\nset NUVIRA_BENCH_SOAK_SECONDS=%s\r\nset NUVIRA_BENCH_SOAK_OUT=%s\\%s\\go-soak.json\r\n' \
                "$secs" "$REMOTE_DIR" "$o"
            printf '%s\\relay.test.exe -test.run "^TestBenchSoakRelay$" -test.v -test.timeout 90m > %s\\go-soak.txt 2>&1\r\n' "$side" "$o"
        fi
        printf 'echo done > %s\\done.txt\r\n' "$o"
    } >"$file"
}

LOCAL_TMP="$OUT/remote-cmd"
mkdir -p "$LOCAL_TMP"

# Setup: fixtures check with both binaries, input files from A's generator.
{
    printf '@echo off\r\ncd /d "%s"\r\nset TMP=%s\\tmp\r\nset TEMP=%s\\tmp\r\n' "$REMOTE_DIR" "$REMOTE_DIR" "$REMOTE_DIR"
    printf 'A\\nuvira-bench.exe fixtures verify --quick > setup.log 2>&1 || (echo fail-A > setup-done.txt & exit /b 2)\r\n'
    printf 'B\\nuvira-bench.exe fixtures verify --quick >> setup.log 2>&1 || (echo fail-B > setup-done.txt & exit /b 2)\r\n'
    if has_word "$SUITES" macro || has_word "$SUITES" res; then
        max=104857600; [ "$TIER" = full ] && max=1073741824
        printf 'A\\nuvira-bench.exe fixtures files --dir inputs\\files --max %s >> setup.log 2>&1 || (echo fail-files > setup-done.txt & exit /b 2)\r\n' "$max"
    fi
    if has_word "$SUITES" micro; then
        # B-MICRO-08/09: decode streams encoded once by A, decoded by both sides.
        printf 'A\\nuvira-bench.exe fixtures decode-streams --dir inputs --tier %s >> setup.log 2>&1 || (echo fail-decode > setup-done.txt & exit /b 2)\r\n' "$TIER"
    fi
    printf 'echo ok > setup-done.txt\r\n'
} >"$LOCAL_TMP/setup.cmd"
$SCP "$LOCAL_TMP/setup.cmd" "$REMOTE:$RD_SCP/setup.cmd" || die "copy of setup.cmd failed"
rsh "del /q \"$REMOTE_DIR\\setup-done.txt\" 2>nul" >/dev/null || true
rsh "cmd /c \"$REMOTE_DIR\\setup.cmd\"" >/dev/null 2>&1
rsh "type \"$REMOTE_DIR\\setup-done.txt\"" 2>/dev/null | grep -q ok || {
    $SCP "$REMOTE:$RD_SCP/setup.log" "$OUT/remote-setup.log" 2>/dev/null
    die "fixtures or inputs differ on $REMOTE (see $OUT/remote-setup.log)"
}

# Timeout of one run: generous (a full tier takes hours).
RUN_TIMEOUT=$((4 * 3600))
INCOMPLETE=0

start_run() {
    # start_run SIDE ROUND
    local name="run-$1-r$2.cmd"
    write_cmd "$1" "$2" "$LOCAL_TMP/$name"
    $SCP "$LOCAL_TMP/$name" "$REMOTE:$RD_SCP/$name" || return 1
    if [ "$MODE" = task ]; then
        rsh "schtasks /create /tn NuviraBench /tr \"$REMOTE_DIR\\$name\" /sc once /st 23:59 /it /f >nul && schtasks /run /tn NuviraBench >nul"
    else
        rsh "cmd /c \"$REMOTE_DIR\\$name\""
    fi
}

wait_run() {
    # wait_run SIDE ROUND
    local marker="$REMOTE_DIR\\out\\$1\\r$2\\done.txt" waited=0
    while [ "$waited" -lt "$RUN_TIMEOUT" ]; do
        rsh "if exist \"$marker\" echo ready" 2>/dev/null | grep -q ready && return 0
        sleep 10
        waited=$((waited + 10))
    done
    return 1
}

fetch_run() {
    # fetch_run SIDE ROUND
    local side="$1" k="$2" rdir="$OUT/$1/r$2" raws f
    mkdir -p "$OUT/$side"
    $SCP -r "$REMOTE:$RD_SCP/out/$side/r$k" "$OUT/$side/" || return 1
    raws=""
    for f in "$rdir"/go-relay.txt "$rdir"/go-signal.txt "$rdir"/go-store.txt; do
        [ -f "$f" ] && raws="$raws $(native "$f")"
    done
    if [ -n "$raws" ]; then
        # shellcheck disable=SC2086
        "$JUDGE" import-go --src "$(native "$BENCH_SRC/go")" --side "$side" --round "$k" \
            --set tier="$TIER" --set machine_alias="$MACHINE" --out "$(native "$rdir/go.json")" $raws || INCOMPLETE=1
    fi
}

k=1
while [ "$k" -le "$ROUNDS" ]; do
    if [ $((k % 2)) -eq 1 ]; then order="A B"; else order="B A"; fi
    for side in $order; do
        say "round $k side $side on $REMOTE ($MODE)"
        start_run "$side" "$k" || { INCOMPLETE=1; continue; }
        if wait_run "$side" "$k"; then
            fetch_run "$side" "$k" || INCOMPLETE=1
        else
            echo "round $k side $side did not finish within $RUN_TIMEOUT s" >&2
            INCOMPLETE=1
        fi
        [ "$IDLE" -gt 0 ] && sleep "$IDLE"
    done
    k=$((k + 1))
done
[ "$MODE" = task ] && rsh "schtasks /delete /tn NuviraBench /f >nul" >/dev/null 2>&1
exit $((INCOMPLETE * 2))
