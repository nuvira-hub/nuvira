#!/usr/bin/env bash
# Builds the Windows Installer package: dist/nuvira.msi, for Intune, Group Policy and
# RMM tools (docs/30-msi-deployment.md). The package carries target/release/nuvira.exe,
# signed first, and is signed itself.
#
#   tools/build-msi.sh              after a release build of the app (build-setup.sh calls it)
#
# Windows only: the package is written through Windows Installer (msi.dll).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PROGRAM="$ROOT/target/release/nuvira.exe"
[ -f "$PROGRAM" ] || { echo "$PROGRAM missing: build the app first" >&2; exit 1; }
(cd "$ROOT/client" && cargo build --release -p nuvira-msi)
"$ROOT/tools/sign-code.sh" "$PROGRAM"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
mkdir -p "$ROOT/dist"
echo "== building the MSI package ($VERSION)"
"$ROOT/target/release/nuvira-msi.exe" --program "$PROGRAM" --version "$VERSION" --out "$ROOT/dist/nuvira.msi"
"$ROOT/tools/sign-code.sh" "$ROOT/dist/nuvira.msi"
ls -la "$ROOT/dist/nuvira.msi"
