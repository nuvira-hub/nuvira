#!/usr/bin/env bash
# Signs the Linux and macOS updates built by the CI, next to the Windows one of
# tools/publish-update.sh (run that first: it empties dist/update/):
#
#   gh run download <linux run> -n nuvira-linux-x64 -D dist/ci
#   gh run download <macos run> -n nuvira-macos -D dist/ci
#   tools/sign-unix-updates.sh dist/ci
#
# Channels written into dist/update/ (docs/19-updates.md):
#   linux-x64      the program, from nuvira-linux-x64.tar.gz
#   linux-x64-deb  nuvira-linux-x64.deb
#   linux-x64-rpm  nuvira-linux-x64.rpm
#   macos          nuvira-macos-app.zip (the signed NuviraHub.app)
# A missing file is skipped. dist/nuvira-update-VERSION.zip is made again with
# every channel, for the GitHub release.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CI="${1:?folder with the CI artifacts}"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
RELEASE="$ROOT/target/release/nuvira-release"
[ -x "$RELEASE.exe" ] && RELEASE="$RELEASE.exe"
[ -x "$RELEASE" ] || (cd "$ROOT" && cargo build --release -p nuvira-update)
OUT="$ROOT/dist/update"
mkdir -p "$OUT"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

sign() {
    echo "== $2"
    "$RELEASE" sign "$1" --platform "$2" --version "$VERSION" --out "$OUT"
}

if [ -f "$CI/nuvira-linux-x64.tar.gz" ]; then
    tar -xzf "$CI/nuvira-linux-x64.tar.gz" -C "$work"
    program="$work/nuvira-$VERSION/nuvira"
    [ -f "$program" ] || { echo "the tar.gz is not version $VERSION"; exit 1; }
    sign "$program" linux-x64
fi
[ -f "$CI/nuvira-linux-x64.deb" ] && sign "$CI/nuvira-linux-x64.deb" linux-x64-deb
[ -f "$CI/nuvira-linux-x64.rpm" ] && sign "$CI/nuvira-linux-x64.rpm" linux-x64-rpm
[ -f "$CI/nuvira-macos-app.zip" ] && sign "$CI/nuvira-macos-app.zip" macos

ZIP="$ROOT/dist/nuvira-update-$VERSION.zip"
rm -f "$ZIP"
if [ -x /c/Windows/System32/tar.exe ]; then
    # Windows' bsdtar writes "/" in the paths (see publish-update.sh).
    (cd "$OUT" && /c/Windows/System32/tar.exe -a -cf "$(cygpath -w "$ZIP")" *)
else
    (cd "$OUT" && zip -qr "$ZIP" .)
fi
echo "== $ZIP"
ls -R "$OUT"
