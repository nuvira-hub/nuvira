#!/usr/bin/env bash
# Test between two PCs in a LAN: agent on this PC, viewer on the remote PC over SSH.
#
#   tools/lan-test.sh [seconds] [extra agent options...]
#   e.g. tools/lan-test.sh 15 --encoder software
#
# The viewer is started in the remote user's Windows session through a scheduled
# task (a window cannot be opened directly from SSH), in view-only mode, and
# writes its results as JSON.
#
# Variables: NUVIRA_REMOTE (user@pc), NUVIRA_REMOTE_DIR, NUVIRA_AGENT_IP, NUVIRA_SSH_KEY,
# NUVIRA_VIEWER_LOG (viewer RUST_LOG, e.g. info,nuvira_viewer=trace),
# NUVIRA_AGENT_EXE (agent to test instead of target/release, for A/B comparisons),
# NUVIRA_VIA_SERVER=1 (starts nuvira-server here and the viewer connects by ID through the server),
# NUVIRA_SERVER (with NUVIRA_VIA_SERVER: use this server, e.g. the NAS container, instead of one on this PC),
# NUVIRA_FORCE_RELAY=1 (with NUVIRA_VIA_SERVER: no direct path, relay only),
# NUVIRA_VIEWER_ARGS (extra viewer options, e.g. "--transport websocket"),
# NUVIRA_VIEWER_SERVER (server address for the viewer, e.g. a wrong UDP port),
# NUVIRA_AGENT_LOG (agent RUST_LOG), NUVIRA_ANIMATE=1 (full-screen animation
# on this PC during the test, to measure capture of the real screen;
# needs cargo build --release --examples), NUVIRA_SNAPSHOT=1 (the viewer saves
# its last frame as snapshot.png: off by default, it shows this PC's screen).

set -euo pipefail

DURATION="${1:-15}"
shift || true
REMOTE="${NUVIRA_REMOTE:?set NUVIRA_REMOTE=user@pc (the remote PC)}"
REMOTE_DIR="${NUVIRA_REMOTE_DIR:?set NUVIRA_REMOTE_DIR (folder on the remote PC, e.g. C:\Users\me\nuvira-test)}"
AGENT_IP="${NUVIRA_AGENT_IP:?set NUVIRA_AGENT_IP (LAN address of this PC, where the agent runs)}"
KEY="${NUVIRA_SSH_KEY:-$HOME/.ssh/nuvira_test}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/target/lan-test"
SSH=(ssh -i "$KEY" -o BatchMode=yes -o ConnectTimeout=8 -o LogLevel=ERROR "$REMOTE")
SCP=(scp -q -i "$KEY" -o BatchMode=yes -o LogLevel=ERROR)
REMOTE_DIR_SCP="/${REMOTE_DIR//\\//}"

mkdir -p "$OUT"
rm -rf "${OUT:?}"/*

echo "== copying the viewer to $REMOTE"
"${SSH[@]}" "if not exist \"$REMOTE_DIR\" mkdir \"$REMOTE_DIR\"" >/dev/null
"${SSH[@]}" "del /q \"$REMOTE_DIR\\stats.json\" \"$REMOTE_DIR\\viewer.log\" \"$REMOTE_DIR\\snapshot.png\" 2>nul" >/dev/null || true
"${SCP[@]}" "$ROOT/target/release/nuvira-viewer.exe" "$REMOTE:$REMOTE_DIR_SCP/nuvira-viewer.exe"

if [ -n "${NUVIRA_ANIMATE:-}" ]; then
    echo "== full-screen animation on this PC"
    "$ROOT/target/release/examples/animate.exe" $((DURATION + 15)) >/dev/null 2>&1 &
    ANIM_PID=$!
    sleep 1
fi

# The Windows firewall rules apply to the programs in dist/: those copies are run.
mkdir -p "$ROOT/dist"
AGENT_ARGS=()
if [ -n "${NUVIRA_VIA_SERVER:-}" ]; then
    if [ -n "${NUVIRA_SERVER:-}" ]; then
        echo "== using nuvira-server at $NUVIRA_SERVER"
    elif tasklist //fi "imagename eq nuvira-server.exe" | grep -qi nuvira-server; then
        # Already started (e.g. for PC-LABORATORIO's service): that one is used.
        echo "== nuvira-server already running on this PC"
    else
        echo "== starting nuvira-server on this PC"
        cp "$ROOT/target/nuvira-server.exe" "$ROOT/dist/nuvira-server.exe"
        # Server data kept between tests: agent and viewer remember its certificate
        # fingerprint and would reject a server with a new certificate.
        mkdir -p "$ROOT/target/lan-test-server"
        NUVIRA_DATA_DIR="$ROOT/target/lan-test-server" "$ROOT/dist/nuvira-server.exe" >"$OUT/server.log" 2>&1 &
        SERVER_PID=$!
        sleep 1
    fi
    SERVER_ADDR="${NUVIRA_SERVER:-$AGENT_IP}"
    AGENT_ARGS=(--server "$SERVER_ADDR")
fi

echo "== starting the agent ($*)"
cp "${NUVIRA_AGENT_EXE:-$ROOT/target/release/nuvira-agent.exe}" "$ROOT/dist/nuvira-agent.exe"
RUST_LOG="${NUVIRA_AGENT_LOG:-info}" "$ROOT/dist/nuvira-agent.exe" --once --view-only --fingerprint-file "$OUT/fingerprint.txt" \
    "${AGENT_ARGS[@]}" "$@" >"$OUT/agent.log" 2>&1 &
AGENT_PID=$!
trap 'kill $AGENT_PID ${ANIM_PID:-} ${SERVER_PID:-} 2>/dev/null || true' EXIT
for _ in $(seq 100); do
    [ -s "$OUT/fingerprint.txt" ] && break
    sleep 0.1
done
FP="$(cat "$OUT/fingerprint.txt")"
if [ -n "${NUVIRA_VIA_SERVER:-}" ]; then
    # In server mode the agent writes "ID password".
    read -r AGENT_ID PASSWORD <<<"$FP"
    VIEWER_TARGET="--server ${NUVIRA_VIEWER_SERVER:-$SERVER_ADDR} $AGENT_ID --password $PASSWORD ${NUVIRA_VIEWER_ARGS:-}"
else
    VIEWER_TARGET="$AGENT_IP --fingerprint $FP"
fi

echo "== starting the viewer for $DURATION s"
printf '@echo off\r\nset NUVIRA_FORCE_RELAY=%s\r\nset RUST_LOG=%s\r\ncd /d "%s"\r\nnuvira-viewer.exe %s --view-only --duration %s --stats-file stats.json %s > viewer.log 2>&1\r\n' \
    "${NUVIRA_FORCE_RELAY:-}" "${NUVIRA_VIEWER_LOG:-info}" "$REMOTE_DIR" "$VIEWER_TARGET" "$DURATION" \
    "${NUVIRA_SNAPSHOT:+--snapshot snapshot.png}" >"$OUT/run-viewer.cmd"
"${SCP[@]}" "$OUT/run-viewer.cmd" "$REMOTE:$REMOTE_DIR_SCP/run-viewer.cmd"
"${SSH[@]}" "schtasks /create /tn NuviraViewerTest /tr \"$REMOTE_DIR\\run-viewer.cmd\" /sc once /st 23:59 /it /f >nul && schtasks /run /tn NuviraViewerTest >nul"

# Waits for the viewer to finish (results file), with a margin.
for _ in $(seq $((DURATION + 30))); do
    sleep 1
    "${SSH[@]}" "if exist \"$REMOTE_DIR\\stats.json\" echo ready" 2>/dev/null | grep -q ready && break
done
sleep 1

"${SCP[@]}" "$REMOTE:$REMOTE_DIR_SCP/stats.json" "$OUT/stats.json" 2>/dev/null || echo "(no stats.json)"
"${SCP[@]}" "$REMOTE:$REMOTE_DIR_SCP/viewer.log" "$OUT/viewer.log" 2>/dev/null || true
"${SCP[@]}" "$REMOTE:$REMOTE_DIR_SCP/snapshot.png" "$OUT/snapshot.png" 2>/dev/null || true
"${SSH[@]}" "schtasks /delete /tn NuviraViewerTest /f >nul" || true

echo "== viewer.log"
sed 's/\x1b\[[0-9;]*m//g' "$OUT/viewer.log" 2>/dev/null | grep -v "^NuviraHub ©" || true
echo "== agent.log"
sed 's/\x1b\[[0-9;]*m//g' "$OUT/agent.log" | grep -v "OpenH264\|^NuviraHub ©" || true
echo "== server.log"
cat "$OUT/server.log" 2>/dev/null | grep -v "^NuviraHub ©" || true
echo "== stats.json"
cat "$OUT/stats.json" 2>/dev/null || true
