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
# Variabili: NEXODESK_REMOTE (utente@pc), NEXODESK_REMOTE_DIR, NEXODESK_AGENT_IP, NEXODESK_SSH_KEY,
# NEXODESK_VIEWER_LOG (RUST_LOG del viewer, es. info,nexodesk_viewer=trace),
# NEXODESK_AGENT_EXE (agent da provare al posto di target/release, per confronti A/B),
# NEXODESK_VIA_SERVER=1 (avvia nexodesk-server qui e il viewer si collega per ID attraverso il server),
# NEXODESK_FORCE_RELAY=1 (con NEXODESK_VIA_SERVER: niente via diretta, solo relay),
# NEXODESK_VIEWER_ARGS (opzioni in più per il viewer, es. "--transport websocket"),
# NEXODESK_VIEWER_SERVER (indirizzo del server per il viewer, es. una porta UDP sbagliata),
# NEXODESK_AGENT_LOG (RUST_LOG dell'agent), NEXODESK_ANIMATE=1 (animazione a schermo intero
# su questo PC durante il test, per misurare la cattura dello schermo vero;
# richiede cargo build --release --examples).

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

echo "== copia del viewer su $REMOTE"
"${SSH[@]}" "if not exist \"$REMOTE_DIR\" mkdir \"$REMOTE_DIR\"" >/dev/null
"${SSH[@]}" "del /q \"$REMOTE_DIR\\stats.json\" \"$REMOTE_DIR\\viewer.log\" \"$REMOTE_DIR\\snapshot.png\" 2>nul" >/dev/null || true
"${SCP[@]}" "$ROOT/target/release/nexodesk-viewer.exe" "$REMOTE:$REMOTE_DIR_SCP/nexodesk-viewer.exe"

if [ -n "${NEXODESK_ANIMATE:-}" ]; then
    echo "== animazione a schermo intero su questo PC"
    "$ROOT/target/release/examples/animate.exe" $((DURATION + 15)) >/dev/null 2>&1 &
    ANIM_PID=$!
    sleep 1
fi

# Le regole del firewall di Windows valgono per i programmi in dist/: si lanciano quelle copie.
mkdir -p "$ROOT/dist"
AGENT_ARGS=()
if [ -n "${NEXODESK_VIA_SERVER:-}" ]; then
    if tasklist //fi "imagename eq nexodesk-server.exe" | grep -qi nexodesk-server; then
        # Già avviato (ad esempio per il servizio di PC-LABORATORIO): si usa quello.
        echo "== nexodesk-server già in esecuzione su questo PC"
    else
        echo "== avvio di nexodesk-server su questo PC"
        cp "$ROOT/target/nexodesk-server.exe" "$ROOT/dist/nexodesk-server.exe"
        # Dati del server conservati tra le prove: agent e viewer ricordano l'impronta
        # del suo certificato e rifiuterebbero un server con un certificato nuovo.
        mkdir -p "$ROOT/target/lan-test-server"
        NEXODESK_DATA_DIR="$ROOT/target/lan-test-server" "$ROOT/dist/nexodesk-server.exe" >"$OUT/server.log" 2>&1 &
        SERVER_PID=$!
        sleep 1
    fi
    AGENT_ARGS=(--server "$AGENT_IP")
fi

echo "== avvio dell'agent ($*)"
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
    # In modalità server l'agent scrive "ID password".
    read -r AGENT_ID PASSWORD <<<"$FP"
    VIEWER_TARGET="--server ${NEXODESK_VIEWER_SERVER:-$AGENT_IP} $AGENT_ID --password $PASSWORD ${NEXODESK_VIEWER_ARGS:-}"
else
    VIEWER_TARGET="$AGENT_IP --fingerprint $FP"
fi

echo "== avvio del viewer per $DURATION s"
printf '@echo off\r\nset NEXODESK_FORCE_RELAY=%s\r\nset RUST_LOG=%s\r\ncd /d "%s"\r\nnexodesk-viewer.exe %s --view-only --duration %s --stats-file stats.json --snapshot snapshot.png > viewer.log 2>&1\r\n' \
    "${NEXODESK_FORCE_RELAY:-}" "${NEXODESK_VIEWER_LOG:-info}" "$REMOTE_DIR" "$VIEWER_TARGET" "$DURATION" >"$OUT/run-viewer.cmd"
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
sed 's/\x1b\[[0-9;]*m//g' "$OUT/viewer.log" 2>/dev/null | grep -v "^NexoDesk ©" || true
echo "== agent.log"
sed 's/\x1b\[[0-9;]*m//g' "$OUT/agent.log" | grep -v "OpenH264\|^NexoDesk ©" || true
echo "== server.log"
cat "$OUT/server.log" 2>/dev/null | grep -v "^NexoDesk ©" || true
echo "== stats.json"
cat "$OUT/stats.json" 2>/dev/null || true
