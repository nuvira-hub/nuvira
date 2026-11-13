#!/usr/bin/env bash
# Builds the signed apt and dnf repositories of NuviraHub (deploy/repo) with the
# .deb and .rpm of a GitHub release, on a Linux host with podman or docker:
#
#   tools/publish-repo.sh VERSION REPO_DIR [KEY]
#
# REPO_DIR keeps the repositories between versions (the last 3 stay), e.g. a copy
# of what the website serves under /apt and /rpm; KEY is the private signing key
# (default ~/.nuvira/repo-signing.asc, see docs/19-updates.md). The key is
# mounted read-only into the container only for the run.
set -euo pipefail

VERSION="${1:?version, e.g. 0.25.0}"
REPO="$(mkdir -p "${2:?repository folder}" && cd "$2" && pwd)"
KEY="${3:-$HOME/.nuvira/repo-signing.asc}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENGINE="$(command -v podman || command -v docker)"

IN="$(mktemp -d)"
KEYDIR="$(mktemp -d)"
trap 'rm -rf "$IN" "$KEYDIR"' EXIT
for f in nuvira-linux-x64.deb nuvira-linux-x64.rpm; do
    curl -fsSL -o "$IN/$f" "https://github.com/nuvira-hub/nuvira/releases/download/v$VERSION/$f"
done
cp "$KEY" "$KEYDIR/repo-signing.asc"
cp "$ROOT"/deploy/repo/nuvirahub.{gpg,asc,sources,repo} "$KEYDIR/"
chmod 600 "$KEYDIR/repo-signing.asc"

"$ENGINE" run --rm \
    -v "$IN:/in:ro" -v "$KEYDIR:/key:ro" -v "$REPO:/repo" -v "$ROOT/deploy/repo/build-repo.sh:/build-repo.sh:ro" \
    docker.io/library/debian:12 sh /build-repo.sh
