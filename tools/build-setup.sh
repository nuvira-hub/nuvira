#!/usr/bin/env bash
# Builds the Windows setup: dist/nuvira-setup.exe.
#
#   tools/build-setup.sh            release build of the app, then the setup
#
# NSIS 3 is downloaded once into target/tools (the same archive Tauri uses),
# checked against its SHA-256.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TOOLS="$ROOT/target/tools"
NSIS_URL="https://github.com/tauri-apps/binary-releases/releases/download/nsis-3/nsis-3.zip"
NSIS_SHA256="1bb9fc85ee5b220d3869325dbb9d191dfe6537070f641c30fbb275c97051fd0c"
MAKENSIS="$TOOLS/nsis/nsis-3.08/makensis.exe"

if [ ! -x "$MAKENSIS" ]; then
    echo "== downloading NSIS"
    mkdir -p "$TOOLS/nsis"
    curl -sSL -o "$TOOLS/nsis-3.zip" "$NSIS_URL"
    echo "$NSIS_SHA256  $TOOLS/nsis-3.zip" | sha256sum -c -
    unzip -q -o "$TOOLS/nsis-3.zip" -d "$TOOLS/nsis"
fi

echo "== building nuvira.exe"
(cd "$ROOT/client" && cargo build --release -p nuvira-app)
"$ROOT/tools/sign-code.sh" "$ROOT/target/release/nuvira.exe"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
mkdir -p "$ROOT/dist"
echo "== building the setup ($VERSION)"
# The script is UTF-8 (accented strings): without this NSIS reads it in the Windows codepage.
(cd "$ROOT/deploy/windows" && "$MAKENSIS" -V2 -INPUTCHARSET UTF8 \
    -DPROGRAM="$(cygpath -w "$ROOT/target/release/nuvira.exe")" \
    -DVERSION="$VERSION" \
    -DOUTFILE="$(cygpath -w "$ROOT/dist/nuvira-setup.exe")" \
    nuvira-setup.nsi)
"$ROOT/tools/sign-code.sh" "$ROOT/dist/nuvira-setup.exe"
ls -la "$ROOT/dist/nuvira-setup.exe"
