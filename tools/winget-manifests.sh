#!/usr/bin/env bash
# Writes the winget manifests of a GitHub release (docs/19-updates.md, "winget"):
# the three files of microsoft/winget-pkgs for NuviraHub.NuviraHub, pointing at the
# release's signed nuvira-setup.exe (NSIS: winget runs it with /S).
#
#   tools/winget-manifests.sh VERSION [OUT_DIR]
#
# OUT_DIR defaults to dist/winget; the files land in
# OUT_DIR/manifests/n/NuviraHub/NuviraHub/VERSION, the layout of winget-pkgs. The
# setup's SHA-256 is read from the release's SHA256SUMS.txt (gh release download).
# Check them with `winget validate --manifest DIR` and `winget install --manifest DIR`.
set -euo pipefail
VERSION="${1:?usage: tools/winget-manifests.sh VERSION [OUT_DIR]}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${2:-$ROOT/dist/winget}"
REPO="nuvira-hub/nuvira"
ID="NuviraHub.NuviraHub"
URL="https://github.com/$REPO/releases/download/v$VERSION/nuvira-setup.exe"

SUMS="$(gh release download "v$VERSION" -R "$REPO" -p SHA256SUMS.txt -O -)"
SHA="$(printf '%s\n' "$SUMS" | awk '$2 == "nuvira-setup.exe" || $2 == "*nuvira-setup.exe" { print toupper($1) }')"
[ -n "$SHA" ] || { echo "nuvira-setup.exe not in the SHA256SUMS.txt of v$VERSION" >&2; exit 1; }
DATE="$(gh release view "v$VERSION" -R "$REPO" --json publishedAt --jq '.publishedAt[:10]')"

DIR="$OUT/manifests/n/NuviraHub/NuviraHub/$VERSION"
mkdir -p "$DIR"
SCHEMA="1.28.0"
HEADER="# Created with tools/winget-manifests.sh of $REPO"

cat > "$DIR/$ID.yaml" <<EOF
$HEADER
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.$SCHEMA.schema.json

PackageIdentifier: $ID
PackageVersion: $VERSION
DefaultLocale: en-US
ManifestType: version
ManifestVersion: $SCHEMA
EOF

# The setup installs for the whole PC (it asks for administrator rights itself) and
# writes the installed apps entry "nuvira" through nuvira.exe service install; the
# program then updates itself from its server, so winget installs over it.
cat > "$DIR/$ID.installer.yaml" <<EOF
$HEADER
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.$SCHEMA.schema.json

PackageIdentifier: $ID
PackageVersion: $VERSION
InstallerType: nullsoft
Scope: machine
ElevationRequirement: elevatesSelf
UpgradeBehavior: install
MinimumOSVersion: 10.0.17763.0
ReleaseDate: $DATE
ProductCode: nuvira
AppsAndFeaturesEntries:
- DisplayName: NuviraHub
  Publisher: Alessandro Blason
  ProductCode: nuvira
InstallationMetadata:
  DefaultInstallLocation: '%ProgramFiles%\nuvira'
Installers:
- Architecture: x64
  InstallerUrl: $URL
  InstallerSha256: $SHA
ManifestType: installer
ManifestVersion: $SCHEMA
EOF

cat > "$DIR/$ID.locale.en-US.yaml" <<EOF
$HEADER
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.$SCHEMA.schema.json

PackageIdentifier: $ID
PackageVersion: $VERSION
PackageLocale: en-US
Publisher: Alessandro Blason
PublisherUrl: https://nuvirahub.dev
PublisherSupportUrl: https://github.com/$REPO/issues
Author: Alessandro Blason
PackageName: NuviraHub
PackageUrl: https://nuvirahub.dev/en/download/
License: AGPL-3.0-or-later
LicenseUrl: https://github.com/$REPO/blob/main/LICENSE
Copyright: Copyright (c) $(date +%Y) Alessandro Blason
ShortDescription: Open source remote desktop with unattended access and an optional self-hosted server.
Description: |-
  NuviraHub controls another computer with an ID and a password, without router setup: the connection is
  direct between the two PCs when the network allows it, otherwise through the server, always end-to-end
  encrypted. The setup installs unattended access (a service) with the NuviraHub public server; the server
  can be your own (one container, one port).
Moniker: nuvirahub
Tags:
- remote-desktop
- remote-access
- remote-support
- screen-sharing
- self-hosted
- open-source
ReleaseNotesUrl: https://github.com/$REPO/releases/tag/v$VERSION
ManifestType: defaultLocale
ManifestVersion: $SCHEMA
EOF

echo "== $DIR"
ls "$DIR"
