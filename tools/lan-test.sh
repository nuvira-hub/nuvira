#!/usr/bin/env bash
# Prova tra due PC in LAN: agent su questo PC, viewer sul PC remoto via SSH.
#
#   tools/lan-test.sh [secondi] [opzioni extra dell'agent...]
#   es. tools/lan-test.sh 15 --encoder software
#
# Il viewer viene avviato nella sessione Windows dell'utente remoto tramite
# un'attività pianificata (una finestra non si può aprire direttamente da SSH),
# in sola visualizzazione, e scrive i risultati in JSON.
#
# Variabili: NEXO_REMOTE (utente@pc), NEXO_REMOTE_DIR, NEXO_AGENT_IP, NEXO_SSH_KEY,
# NEXO_VIEWER_LOG (RUST_LOG del viewer, es. info,nexo_viewer=trace),
# NEXO_AGENT_EXE (agent da provare al posto di target/release, per confronti A/B),
# NEXO_AGENT_LOG (RUST_LOG dell'agent).

set -euo pipefail

DURATION="${1:-15}"
shift || true
REMOTE="${NEXO_REMOTE:-mrbla@PC-LABORATORIO}"
REMOTE_DIR="${NEXO_REMOTE_DIR:-C:\\Users\\mrbla\\NexoTest}"
AGENT_IP="${NEXO_AGENT_IP:-192.168.178.185}"
KEY="${NEXO_SSH_KEY:-$HOME/.ssh/nexo_test}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/target/lan-test"
SSH=(ssh -i "$KEY" -o BatchMode=yes -o ConnectTimeout=8 -o LogLevel=ERROR "$REMOTE")
SCP=(scp -q -i "$KEY" -o BatchMode=yes -o LogLevel=ERROR)
REMOTE_DIR_SCP="/${REMOTE_DIR//\\//}"

mkdir -p "$OUT"
rm -f "$OUT"/*

echo "== copia del viewer su $REMOTE"
"${SSH[@]}" "if not exist \"$REMOTE_DIR\" mkdir \"$REMOTE_DIR\"" >/dev/null
"${SSH[@]}" "del /q \"$REMOTE_DIR\\stats.json\" \"$REMOTE_DIR\\viewer.log\" \"$REMOTE_DIR\\snapshot.png\" 2>nul" >/dev/null || true
"${SCP[@]}" "$ROOT/target/release/nexo-viewer.exe" "$REMOTE:$REMOTE_DIR_SCP/nexo-viewer.exe"

echo "== avvio dell'agent ($*)"
# La regola del firewall di Windows vale per dist/nexo-agent.exe: si lancia quella copia.
mkdir -p "$ROOT/dist"
cp "${NEXO_AGENT_EXE:-$ROOT/target/release/nexo-agent.exe}" "$ROOT/dist/nexo-agent.exe"
RUST_LOG="${NEXO_AGENT_LOG:-info}" "$ROOT/dist/nexo-agent.exe" --once --view-only --fingerprint-file "$OUT/fingerprint.txt" "$@" \
    >"$OUT/agent.log" 2>&1 &
AGENT_PID=$!
trap 'kill $AGENT_PID 2>/dev/null || true' EXIT
for _ in $(seq 50); do
    [ -s "$OUT/fingerprint.txt" ] && break
    sleep 0.1
done
FP="$(cat "$OUT/fingerprint.txt")"

echo "== avvio del viewer per $DURATION s"
printf '@echo off\r\nset RUST_LOG=%s\r\ncd /d "%s"\r\nnexo-viewer.exe %s --fingerprint %s --view-only --duration %s --stats-file stats.json --snapshot snapshot.png > viewer.log 2>&1\r\n' \
    "${NEXO_VIEWER_LOG:-info}" "$REMOTE_DIR" "$AGENT_IP" "$FP" "$DURATION" >"$OUT/run-viewer.cmd"
"${SCP[@]}" "$OUT/run-viewer.cmd" "$REMOTE:$REMOTE_DIR_SCP/run-viewer.cmd"
"${SSH[@]}" "schtasks /create /tn NexoViewerTest /tr \"$REMOTE_DIR\\run-viewer.cmd\" /sc once /st 23:59 /it /f >nul && schtasks /run /tn NexoViewerTest >nul"

# Attende la fine del viewer (file dei risultati) con un margine.
for _ in $(seq $((DURATION + 30))); do
    sleep 1
    "${SSH[@]}" "if exist \"$REMOTE_DIR\\stats.json\" echo pronto" 2>/dev/null | grep -q pronto && break
done
sleep 1

"${SCP[@]}" "$REMOTE:$REMOTE_DIR_SCP/stats.json" "$OUT/stats.json" 2>/dev/null || echo "(nessun stats.json)"
"${SCP[@]}" "$REMOTE:$REMOTE_DIR_SCP/viewer.log" "$OUT/viewer.log" 2>/dev/null || true
"${SCP[@]}" "$REMOTE:$REMOTE_DIR_SCP/snapshot.png" "$OUT/snapshot.png" 2>/dev/null || true
"${SSH[@]}" "schtasks /delete /tn NexoViewerTest /f >nul" || true

echo "== viewer.log"
sed 's/\x1b\[[0-9;]*m//g' "$OUT/viewer.log" 2>/dev/null | grep -v "^Nexo ©" || true
echo "== agent.log"
sed 's/\x1b\[[0-9;]*m//g' "$OUT/agent.log" | grep -v "OpenH264\|^Nexo ©" || true
echo "== stats.json"
cat "$OUT/stats.json" 2>/dev/null || true
