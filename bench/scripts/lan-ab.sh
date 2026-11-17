#!/usr/bin/env bash
# B-MACRO-09: A/B of the real binaries over the real LAN. Agent (A or B) on this PC,
# viewer (same side) on a remote Windows PC with its real window, alternating ABBA.
# Same method as tools/lan-test.sh (viewer started by a scheduled task in the remote
# user's session, --stats-file JSON), which is not modified: this script repeats its
# steps with per-side binaries and writes nuvira-bench/1 JSON for `nuvira-bench compare`.
#
#   bench/scripts/lan-ab.sh --dist DIR --out DIR [--rounds 5] [--duration 20] [--idle 10]
#                           [--case NAME] [-- extra agent options]
#
# DIR/A and DIR/B must hold nuvira-agent.exe and nuvira-viewer.exe (bench-compare.sh
# --with-binaries builds them into $WORK/dist) and nuvira-bench.exe (for compare).
# Default agent options: --synthetic 1920x1080@60 (the screen of this PC is not
# captured). With NUVIRA_ANIMATE=1 and agent options without --synthetic, the real
# screen is captured while DIR/<side>/animate.exe runs full screen: ask the user first.
#
# Variables as tools/lan-test.sh: NUVIRA_REMOTE (user@pc), NUVIRA_REMOTE_DIR,
# NUVIRA_AGENT_IP, NUVIRA_SSH_KEY, NUVIRA_VIDEO_FEC, NUVIRA_DECODER, NUVIRA_VIEWER_ARGS.
# The Windows firewall asks once per agent path (DIR/A, DIR/B): the first run needs the user.
#
# Example (NUC agent -> PC-LABORATORIO viewer):
#   NUVIRA_REMOTE=mrbla@PC-LABORATORIO NUVIRA_REMOTE_DIR='C:\Users\mrbla\nuvira-bench-lan' \
#   NUVIRA_AGENT_IP=192.168.178.185 bench/scripts/lan-ab.sh --dist /d/nuvira-review/dist \
#       --out /d/nuvira-review/runs/lan-ab/$(date -u +%Y-%m-%d)
#   then: /d/nuvira-review/dist/B/nuvira-bench.exe compare --base OUT/A --new OUT/B --markdown OUT/compare.md

set -u
set -o pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_SRC="$(cd "$SCRIPT_DIR/.." && pwd)"
. "$SCRIPT_DIR/lib.sh"

DIST=""; OUT=""; ROUNDS=5; DURATION=20; IDLE=10; CASE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --dist) DIST="$2"; shift ;;
        --out) OUT="$2"; shift ;;
        --rounds) ROUNDS="$2"; shift ;;
        --duration) DURATION="$2"; shift ;;
        --idle) IDLE="$2"; shift ;;
        --case) CASE="$2"; shift ;;
        --) shift; break ;;
        -h|--help) sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "lan-ab: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done
[ -n "$DIST" ] && [ -n "$OUT" ] || { echo "lan-ab: --dist and --out are required" >&2; exit 2; }
AGENT_OPTS="$*"
[ -n "$AGENT_OPTS" ] || AGENT_OPTS="--synthetic 1920x1080@60"
[ -n "$CASE" ] || CASE="lan/$(echo "$AGENT_OPTS" | tr ' ' '_')${NUVIRA_VIDEO_FEC:+/fec}${NUVIRA_DECODER:+/$NUVIRA_DECODER}"

REMOTE="${NUVIRA_REMOTE:?set NUVIRA_REMOTE=user@pc}"
REMOTE_DIR="${NUVIRA_REMOTE_DIR:?set NUVIRA_REMOTE_DIR (folder on the remote PC)}"
AGENT_IP="${NUVIRA_AGENT_IP:?set NUVIRA_AGENT_IP (LAN address of this PC)}"
KEY="${NUVIRA_SSH_KEY:-$HOME/.ssh/nuvira_test}"
SSH="ssh -i $KEY -o BatchMode=yes -o ConnectTimeout=8 -o LogLevel=ERROR $REMOTE"
SCP="scp -q -i $KEY -o BatchMode=yes -o LogLevel=ERROR"
RD_SCP="/$(echo "$REMOTE_DIR" | tr '\\' '/')"
mkdir -p "$OUT"

for side in A B; do
    for b in nuvira-agent nuvira-viewer; do
        [ -f "$DIST/$side/$b.exe" ] || { echo "lan-ab: missing $DIST/$side/$b.exe (bench-compare.sh --with-binaries)" >&2; exit 2; }
    done
    $SSH "if not exist \"$REMOTE_DIR\\$side\" mkdir \"$REMOTE_DIR\\$side\"" >/dev/null
    $SCP "$DIST/$side/nuvira-viewer.exe" "$REMOTE:$RD_SCP/$side/nuvira-viewer.exe" || exit 2
done

json_lines() {
    # Agent 5-second lines as a JSON array of strings (ANSI colours removed).
    sed 's/\x1b\[[0-9;]*m//g' "$1" 2>/dev/null | grep "fps sent" | sed 's/\\/\\\\/g; s/"/\\"/g; s/\t/ /g; s/^/"/; s/$/"/' \
        | paste -sd, - | sed 's/^/[/; s/$/]/'
}

one_run() {
    # one_run SIDE ROUND
    local side="$1" k="$2" rdir="$OUT/$1/r$2" agent_pid anim_pid="" fp status note report lines i
    mkdir -p "$rdir"
    rm -f "$rdir/fingerprint.txt"
    if [ -n "${NUVIRA_ANIMATE:-}" ] && [ -f "$DIST/$side/animate.exe" ]; then
        "$DIST/$side/animate.exe" $((DURATION + 15)) >/dev/null 2>&1 &
        anim_pid=$!
        sleep 1
    fi
    # shellcheck disable=SC2086
    RUST_LOG=info "$DIST/$side/nuvira-agent.exe" --once --view-only --fingerprint-file "$(native "$rdir/fingerprint.txt")" \
        $AGENT_OPTS >"$rdir/agent.log" 2>&1 &
    agent_pid=$!
    i=0
    while [ "$i" -lt 100 ] && [ ! -s "$rdir/fingerprint.txt" ]; do sleep 0.1; i=$((i + 1)); done
    fp="$(cat "$rdir/fingerprint.txt" 2>/dev/null)"
    $SSH "del /q \"$REMOTE_DIR\\$side\\stats.json\" \"$REMOTE_DIR\\$side\\viewer.log\" 2>nul" >/dev/null || true
    printf '@echo off\r\nset NUVIRA_VIDEO_FEC=%s\r\nset NUVIRA_DECODER=%s\r\nset RUST_LOG=info\r\ncd /d "%s\\%s"\r\nnuvira-viewer.exe %s --fingerprint %s --view-only --duration %s --stats-file stats.json %s > viewer.log 2>&1\r\n' \
        "${NUVIRA_VIDEO_FEC:-}" "${NUVIRA_DECODER:-}" "$REMOTE_DIR" "$side" "$AGENT_IP" "$fp" "$DURATION" "${NUVIRA_VIEWER_ARGS:-}" \
        >"$rdir/run-viewer.cmd"
    $SCP "$rdir/run-viewer.cmd" "$REMOTE:$RD_SCP/$side/run-viewer.cmd"
    $SSH "schtasks /create /tn NuviraBenchLan /tr \"$REMOTE_DIR\\$side\\run-viewer.cmd\" /sc once /st 23:59 /it /f >nul && schtasks /run /tn NuviraBenchLan >nul"
    i=0
    while [ "$i" -lt $((DURATION + 40)) ]; do
        sleep 1
        $SSH "if exist \"$REMOTE_DIR\\$side\\stats.json\" echo ready" 2>/dev/null | grep -q ready && break
        i=$((i + 1))
    done
    sleep 1
    $SCP "$REMOTE:$RD_SCP/$side/stats.json" "$rdir/stats.json" 2>/dev/null || true
    $SCP "$REMOTE:$RD_SCP/$side/viewer.log" "$rdir/viewer.log" 2>/dev/null || true
    $SSH "schtasks /delete /tn NuviraBenchLan /f >nul" >/dev/null 2>&1 || true
    kill "$agent_pid" ${anim_pid:+"$anim_pid"} 2>/dev/null || true
    wait "$agent_pid" 2>/dev/null || true

    status=ok; note=""
    if [ -s "$rdir/stats.json" ]; then
        report="$(cat "$rdir/stats.json")"
        grep -q '"completed": true' "$rdir/stats.json" || { status=failed; note="viewer session not completed"; }
    else
        report="null"; status=failed; note="no stats.json from the viewer"
    fi
    lines="$(json_lines "$rdir/agent.log")"
    [ -n "$lines" ] || lines="[]"
    cat >"$rdir/lan.json" <<EOF
{
  "schema": "nuvira-bench/1",
  "run": {"id": "lan-$side-r$k", "side": "$side", "round": $k, "suite": "lan",
          "machine": {"alias": "$(hostname) -> ${REMOTE#*@}", "os": "windows"},
          "args": "$(echo "$AGENT_OPTS" | sed 's/"/\\"/g')", "started": "$(date -u +%Y-%m-%dT%H:%M:%SZ)"},
  "results": [{
    "id": "B-MACRO-09", "case": "$CASE", "level": "macro", "status": "$status",
    "config_ids": ["WIN11", "NET-NAT"],
    "params": {"duration_s": $DURATION, "agent_ip": "$AGENT_IP"},
    "metrics": {},
    "viewer_report": $report,
    "agent_log": $lines,
    "notes": [$( [ -n "$note" ] && echo "\"$note\"" )]
  }]
}
EOF
    echo "== round $k side $side: $status" >&2
}

k=1
while [ "$k" -le "$ROUNDS" ]; do
    if [ $((k % 2)) -eq 1 ]; then order="A B"; else order="B A"; fi
    for side in $order; do
        one_run "$side" "$k"
        [ "$IDLE" -gt 0 ] && sleep "$IDLE"
    done
    k=$((k + 1))
done
echo "== results in $OUT; compare with: $DIST/B/nuvira-bench.exe compare --base $OUT/A --new $OUT/B --markdown $OUT/compare.md --json $OUT/compare.json" >&2
