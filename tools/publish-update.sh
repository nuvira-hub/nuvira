#!/usr/bin/env bash
# Builds nuvira.exe and signs it as an update: dist/update/ holds the files to
# copy into the `updates` folder of the server's data (on Unraid:
# /mnt/user/appdata/nuvira-server/updates). Devices with the service install
# it within 6 hours, outside sessions; portable copies offer it.
#
#   tools/publish-update.sh
#
# The version is the workspace's (Cargo.toml): raise it before publishing, or the
# devices already on it will not update. Signing needs the release key
# (%USERPROFILE%\.nuvira\release-signing.pk8, see nuvira-release keygen).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
echo "== building"
(cd "$ROOT/client" && cargo build --release -p nuvira-app -p nuvira-update)
rm -rf "$ROOT/dist/update"
echo "== signing"
"$ROOT/target/release/nuvira-release.exe" sign "$ROOT/target/release/nuvira.exe" --out "$ROOT/dist/update"
