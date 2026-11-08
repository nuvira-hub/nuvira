#!/usr/bin/env bash
# Builds nuvira.exe and signs it as an update: dist/update/ holds the files to
# copy into the `updates` folder of the server's data (on Unraid:
# /mnt/user/appdata/nuvira-server/updates). Devices with the service install
# it within 6 hours, outside sessions; portable copies offer it.
# dist/nuvira-update-VERSION.zip holds the same files, for the GitHub release.
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
# Authenticode first: the update signature covers the final bytes.
"$ROOT/tools/sign-code.sh" "$ROOT/target/release/nuvira.exe"
echo "== signing the update"
"$ROOT/target/release/nuvira-release.exe" sign "$ROOT/target/release/nuvira.exe" --out "$ROOT/dist/update"
# The same files as one archive for the GitHub release: whoever runs their own
# server unpacks it into the `updates` folder (deploy/README.md, "Updates").
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
ZIP="$ROOT/dist/nuvira-update-$VERSION.zip"
rm -f "$ROOT"/dist/nuvira-update-*.zip
# Windows' own bsdtar: Compress-Archive of PowerShell 5 writes the paths with "\",
# which some Linux tools unpack as file names (a NAS's updates folder, 0.17.0).
(cd "$ROOT/dist/update" && /c/Windows/System32/tar.exe -a -cf "$(cygpath -w "$ZIP")" windows-x64.json windows-x64)
echo "== $ZIP"
