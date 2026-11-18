#!/usr/bin/env bash
# Runs the Xvfb group of the e2e tests (Linux): starts a private Xvfb on a free
# display, removes what could reach the real desktop session (WAYLAND_DISPLAY, the
# session bus, XDG_SESSION_TYPE) and runs the cases with the real X11 capture,
# input and clipboard of that virtual display only.
#
#   e2e/scripts/run-xvfb.sh [cargo test arguments...]     (from the nuvira tree)
#
# Needs: Xvfb, xclip, xdotool (apt install xvfb xclip xdotool). Other variables
# (CARGO_TARGET_DIR, NUVIRA_BENCH_SERVER_EXE, NUVIRA_E2E_REPEAT...) pass through.
set -euo pipefail
cd "$(dirname "$0")/.."
for tool in Xvfb xclip xdotool; do
    command -v "$tool" >/dev/null || { echo "missing $tool (apt install xvfb xclip xdotool)" >&2; exit 2; }
done
display=""
for n in $(seq 90 120); do
    if [ ! -e "/tmp/.X$n-lock" ] && [ ! -e "/tmp/.X11-unix/X$n" ]; then display=":$n"; break; fi
done
[ -n "$display" ] || { echo "no free X display number" >&2; exit 2; }
Xvfb "$display" -screen 0 1280x720x24 -nolisten tcp -noreset >/dev/null 2>&1 &
xvfb_pid=$!
trap 'kill "$xvfb_pid" 2>/dev/null || true' EXIT
for _ in $(seq 50); do
    [ -e "/tmp/.X11-unix/X${display#:}" ] && break
    sleep 0.1
done
env -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS -u XDG_SESSION_TYPE -u XAUTHORITY \
    DISPLAY="$display" NUVIRA_E2E_XVFB=1 NUVIRA_E2E_XVFB_PID="$xvfb_pid" \
    cargo test --release --test e2e "$@" -- xvfb/
