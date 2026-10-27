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
# Variables: NEXODESK_REMOTE (user@pc), NEXODESK_REMOTE_DIR, NEXODESK_AGENT_IP, NEXODESK_SSH_KEY,
# NEXODESK_VIEWER_LOG (viewer RUST_LOG, e.g. info,nexodesk_viewer=trace),
# NEXODESK_AGENT_EXE (agent to test instead of target/release, for A/B comparisons),
# NEXODESK_VIA_SERVER=1 (starts nexodesk-server here and the viewer connects by ID through the server),
# NEXODESK_SERVER (with NEXODESK_VIA_SERVER: use this server, e.g. the NAS container, instead of one on this PC),
# NEXODESK_FORCE_RELAY=1 (with NEXODESK_VIA_SERVER: no direct path, relay only),
# NEXODESK_VIEWER_ARGS (extra viewer options, e.g. "--transport websocket"),
# NEXODESK_VIEWER_SERVER (server address for the viewer, e.g. a wrong UDP port),
# NEXODESK_AGENT_LOG (agent RUST_LOG), NEXODESK_ANIMATE=1 (full-screen animation
# on this PC during the test, to measure capture of the real screen;
# needs cargo build --release --examples).

set -euo pipefail

DURATION="${1:-15}"
shift || true
REMOTE="${NEXODESK_REMOTE:-mrbla@PC-LABORATORIO}"
REMOTE_DIR="${NEXODESK_REMOTE_DIR:-C:\\Users\\mrbla\\NexoTest}"
AGENT_IP="${NEXODESK_AGENT_IP:-192.168.178.185}"
KEY="${NEXODESK_SSH_KEY:-$HOME/.ssh/nexo_test}"
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
"${SCP[@]}" "$ROOT/target/release/nexodesk-viewer.exe" "$REMOTE:$REMOTE_DIR_SCP/nexodesk-viewer.exe"

if [ -n "${NEXODESK_ANIMATE:-}" ]; then
    echo "== full-screen animation on this PC"
    "$ROOT/target/release/examples/animate.exe" $((DURATION + 15)) >/dev/null 2>&1 &
    ANIM_PID=$!
    sleep 1
fi

# The Windows firewall rules apply to the programs in dist/: those copies are run.
mkdir -p "$ROOT/dist"
AGENT_ARGS=()
if [ -n "${NEXODESK_VIA_SERVER:-}" ]; then
    if [ -n "${NEXODESK_SERVER:-}" ]; then
        echo "== using nexodesk-server at $NEXODESK_SERVER"
    elif tasklist //fi "imagename eq nexodesk-server.exe" | grep -qi nexodesk-server; then
        # Already started (e.g. for PC-LABORATORIO's service): that one is used.
        echo "== nexodesk-server already running on this PC"
    else
        echo "== starting nexodesk-server on this PC"
        cp "$ROOT/target/nexodesk-server.exe" "$ROOT/dist/nexodesk-server.exe"
        # Server data kept between tests: agent and viewer remember its certificate
        # fingerprint and would reject a server with a new certificate.
        mkdir -p "$ROOT/target/lan-test-server"
        NEXODESK_DATA_DIR="$ROOT/target/lan-test-server" "$ROOT/dist/nexodesk-server.exe" >"$OUT/server.log" 2>&1 &
        SERVER_PID=$!
        sleep 1
    fi
    SERVER_ADDR="${NEXODESK_SERVER:-$AGENT_IP}"
    AGENT_ARGS=(--server "$SERVER_ADDR")
fi

echo "== starting the agent ($*)"
cp "${NEXODESK_AGENT_EXE:-$ROOT/target/release/nexodesk-agent.exe}" "$ROOT/dist/nexodesk-agent.exe"
RUST_LOG="${NEXODESK_AGENT_LOG:-info}" "$ROOT/dist/nexodesk-agent.exe" --once --view-only --fingerprint-file "$OUT/fingerprint.txt" \
    "${AGENT_ARGS[@]}" "$@" >"$OUT/agent.log" 2>&1 &
AGENT_PID=$!
trap 'kill $AGENT_PID ${ANIM_PID:-} ${SERVER_PID:-} 2>/dev/null || true' EXIT
for _ in $(seq 100); do
    [ -s "$OUT/fingerprint.txt" ] && break
    sleep 0.1
done
FP="$(cat "$OUT/fingerprint.txt")"
if [ -n "${NEXODESK_VIA_SERVER:-}" ]; then
    # In server mode the agent writes "ID password".
    read -r AGENT_ID PASSWORD <<<"$FP"
    VIEWER_TARGET="--server ${NEXODESK_VIEWER_SERVER:-$SERVER_ADDR} $AGENT_ID --password $PASSWORD ${NEXODESK_VIEWER_ARGS:-}"
else
    VIEWER_TARGET="$AGENT_IP --fingerprint $FP"
fi

echo "== starting the viewer for $DURATION s"
printf '@echo off\r\nset NEXODESK_FORCE_RELAY=%s\r\nset RUST_LOG=%s\r\ncd /d "%s"\r\nnexodesk-viewer.exe %s --view-only --duration %s --stats-file stats.json --snapshot snapshot.png > viewer.log 2>&1\r\n' \
    "${NEXODESK_FORCE_RELAY:-}" "${NEXODESK_VIEWER_LOG:-info}" "$REMOTE_DIR" "$VIEWER_TARGET" "$DURATION" >"$OUT/run-viewer.cmd"
"${SCP[@]}" "$OUT/run-viewer.cmd" "$REMOTE:$REMOTE_DIR_SCP/run-viewer.cmd"
"${SSH[@]}" "schtasks /create /tn NexoViewerTest /tr \"$REMOTE_DIR\\run-viewer.cmd\" /sc once /st 23:59 /it /f >nul && schtasks /run /tn NexoViewerTest >nul"

# Waits for the viewer to finish (results file), with a margin.
for _ in $(seq $((DURATION + 30))); do
    sleep 1
    "${SSH[@]}" "if exist \"$REMOTE_DIR\\stats.json\" echo ready" 2>/dev/null | grep -q ready && break
done
sleep 1

"${SCP[@]}" "$REMOTE:$REMOTE_DIR_SCP/stats.json" "$OUT/stats.json" 2>/dev/null || echo "(no stats.json)"
"${SCP[@]}" "$REMOTE:$REMOTE_DIR_SCP/viewer.log" "$OUT/viewer.log" 2>/dev/null || true
"${SCP[@]}" "$REMOTE:$REMOTE_DIR_SCP/snapshot.png" "$OUT/snapshot.png" 2>/dev/null || true
"${SSH[@]}" "schtasks /delete /tn NexoViewerTest /f >nul" || true

echo "== viewer.log"
sed 's/\x1b\[[0-9;]*m//g' "$OUT/viewer.log" 2>/dev/null | grep -v "^NexoDesk ©" || true
echo "== agent.log"
sed 's/\x1b\[[0-9;]*m//g' "$OUT/agent.log" | grep -v "OpenH264\|^NexoDesk ©" || true
echo "== server.log"
cat "$OUT/server.log" 2>/dev/null | grep -v "^NexoDesk ©" || true
echo "== stats.json"
cat "$OUT/stats.json" 2>/dev/null || true
