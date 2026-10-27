#!/usr/bin/env bash
# Builds nexodesk.exe and signs it as an update: dist/update/ holds the files to
# copy into the `updates` folder of the server's data (on Unraid:
# /mnt/user/appdata/nexodesk-server/updates). Devices with the service install
# it within 6 hours, outside sessions; portable copies offer it.
#
#   tools/publish-update.sh
#
# The version is the workspace's (Cargo.toml): raise it before publishing, or the
# devices already on it will not update. Signing needs the release key
# (%USERPROFILE%\.nexodesk\release-signing.pk8, see nexodesk-release keygen).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
echo "== building"
(cd "$ROOT/client" && cargo build --release -p nexodesk-app -p nexodesk-update)
rm -rf "$ROOT/dist/update"
echo "== signing"
"$ROOT/target/release/nexodesk-release.exe" sign "$ROOT/target/release/nexodesk.exe" --out "$ROOT/dist/update"
