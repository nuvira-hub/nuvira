#!/bin/bash
# Builds the macOS package: NuviraHub.app (universal: Apple Silicon and Intel)
# in nuvira-macos.dmg. Runs on a Mac (the GitHub workflow). With the Developer
# ID certificate in the keychain (NUVIRA_SIGN_IDENTITY) the app and the .dmg are
# signed with the hardened runtime; notarization is a separate step.
#
#   tools/package-macos.sh [out_dir]
#
# Needs the Rust targets aarch64-apple-darwin and x86_64-apple-darwin, nasm and
# rsvg-convert (librsvg) for the icon.
set -euo pipefail
cd "$(dirname "$0")/.."

out=${1:-dist}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
identity=${NUVIRA_SIGN_IDENTITY:-}
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
export MACOSX_DEPLOYMENT_TARGET=13.0

for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --release --target "$target" -p nuvira-app -p nuvira-cups
done

app="$stage/dmg/NuviraHub.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$out"
lipo -create -output "$app/Contents/MacOS/nuvira" \
    target/aarch64-apple-darwin/release/nuvira target/x86_64-apple-darwin/release/nuvira
# The CUPS backend of the NuviraHub printer (remote printing), copied into
# /usr/libexec/cups/backend when unattended access is installed.
lipo -create -output "$app/Contents/MacOS/nuvira-cups" \
    target/aarch64-apple-darwin/release/nuvira-cups target/x86_64-apple-darwin/release/nuvira-cups

# Icon: every size of the iconset from the SVG of the app logo.
set_dir="$stage/nuvira.iconset"
mkdir -p "$set_dir"
for size in 16 32 128 256 512; do
    rsvg-convert -w $size -h $size client/app/ui/logo.svg -o "$set_dir/icon_${size}x${size}.png"
    rsvg-convert -w $((size * 2)) -h $((size * 2)) client/app/ui/logo.svg -o "$set_dir/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$set_dir" -o "$app/Contents/Resources/nuvira.icns"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>NuviraHub</string>
    <key>CFBundleDisplayName</key><string>NuviraHub</string>
    <key>CFBundleIdentifier</key><string>io.github.nuvira-hub.nuvira</string>
    <key>CFBundleExecutable</key><string>nuvira</string>
    <key>CFBundleIconFile</key><string>nuvira</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSHumanReadableCopyright</key><string>© 2026 Alessandro Blason · AGPL-3.0</string>
    <key>CFBundleURLTypes</key><array><dict>
        <key>CFBundleURLName</key><string>NuviraHub invitation</string>
        <key>CFBundleURLSchemes</key><array><string>nuvirahub</string></array>
    </dict></array>
    <key>NSMicrophoneUsageDescription</key><string>NuviraHub sends your voice to the computer you control, while you turn on the microphone in the session.</string>
    <key>NSLocalNetworkUsageDescription</key><string>NuviraHub lists the computers of your network that can be controlled ("On this network"), and shows this Mac to them.</string>
    <key>NSBonjourServices</key><array><string>_nuvira._udp</string></array>
</dict>
</plist>
EOF
cp LICENSE "$app/Contents/Resources/"

# The hardened runtime allows the microphone only with this entitlement (the
# technician's microphone, docs/45-microphone.md); macOS asks the person once.
entitlements="$(mktemp)"
cat > "$entitlements" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.device.audio-input</key><true/>
</dict>
</plist>
EOF

# Network Topology Observation: since macOS 27 a program without it gets
# 02:00:00:00:00:00 for every MAC, of its own interfaces and of the neighbours
# (network scan, Wake-on-LAN; nuvira-pro docs/internal/48-network-scan.md).
# A restricted entitlement: only with the Developer ID provisioning profile of the
# app (deploy/macos, made in the Apple Developer account for this bundle ID and
# the Developer ID Application certificate), embedded in the app. Without the
# profile, or without a certificate (ad hoc builds), the app has no such
# entitlement and the MACs stay unknown. An app that claims it with no profile
# authorising it would not start at all: the checks below stop the build instead.
profile=deploy/macos/NuviraHub.provisionprofile
topology=com.apple.developer.networking.topology-observation
if [ -n "$identity" ] && [ -f "$profile" ]; then
    security cms -D -i "$profile" > "$stage/profile.plist"
    pb() { /usr/libexec/PlistBuddy -c "Print :$1" "$stage/profile.plist"; }
    team=$(pb TeamIdentifier:0)
    [ "$(pb Entitlements:com.apple.application-identifier)" = "$team.io.github.nuvira-hub.nuvira" ] \
        || { echo "provisioning profile of another app" >&2; exit 1; }
    [ "$(pb "Entitlements:$topology")" = "true" ] \
        || { echo "provisioning profile without $topology" >&2; exit 1; }
    expires=$(date -j -f "%a %b %d %T %Z %Y" "$(pb ExpirationDate)" +%s)
    [ "$expires" -gt "$(( $(date +%s) + 30 * 86400 ))" ] \
        || { echo "provisioning profile expires within 30 days: make a new one" >&2; exit 1; }
    cp "$profile" "$app/Contents/embedded.provisionprofile"
    /usr/libexec/PlistBuddy \
        -c "Add :com.apple.application-identifier string $team.io.github.nuvira-hub.nuvira" \
        -c "Add :com.apple.developer.team-identifier string $team" \
        -c "Add :$topology bool true" "$entitlements"
fi

# The virtual HID keyboard that types into the lock screen (docs/33-macos.md,
# "Locked screen"): Karabiner-DriverKit-VirtualHIDDevice, public domain, signed and
# notarized by its author (tools/vendor); installed by `nuvira service system`.
KVHID_VERSION=8.7.0
KVHID_SHA256=3364294fca2629d4d8968bbc434537f4093b8ba620a211ec192b3d2e71e42dfb
kvhid="$app/Contents/Resources/Karabiner-DriverKit-VirtualHIDDevice.pkg"
cp "tools/vendor/Karabiner-DriverKit-VirtualHIDDevice-$KVHID_VERSION.pkg" "$kvhid"
echo "$KVHID_SHA256  $kvhid" | shasum -a 256 -c -

if [ -n "$identity" ]; then
    # Nested code first: the app's signature seals it.
    codesign --force --options runtime --timestamp --sign "$identity" "$app/Contents/MacOS/nuvira-cups"
    codesign --force --options runtime --timestamp --entitlements "$entitlements" --sign "$identity" "$app"
    codesign --verify --strict --verbose=2 "$app"
fi

# The signed app alone, for the automatic updates (channel "macos", signed by
# tools/sign-unix-updates.sh); ditto keeps the bundle's signature intact.
ditto -c -k --keepParent "$app" "$out/nuvira-macos-app.zip"

# The package for organisations (MDM: Jamf, Intune, Kandji, Mosyle): the app into
# /Applications, then `nuvira service deploy` for the whole Mac
# (docs/35-macos-deployment.md). Signed with the Developer ID Installer
# certificate when NUVIRA_INSTALLER_IDENTITY names it (MDMs want it signed).
pkgroot="$stage/pkgroot"
mkdir -p "$pkgroot/Applications" "$stage/pkg-scripts"
ditto "$app" "$pkgroot/Applications/NuviraHub.app"
cp deploy/macos/postinstall "$stage/pkg-scripts/postinstall"
chmod 755 "$stage/pkg-scripts/postinstall"
pkgbuild --analyze --root "$pkgroot" "$stage/component.plist"
# Always into /Applications (not wherever a copy of the app was found), and never
# over a newer app that the automatic updates put there.
/usr/libexec/PlistBuddy -c "Set :0:BundleIsRelocatable false" -c "Set :0:BundleIsVersionChecked true" "$stage/component.plist"
pkgbuild --root "$pkgroot" --component-plist "$stage/component.plist" --scripts "$stage/pkg-scripts" \
    --identifier io.github.nuvira-hub.nuvira.pkg --version "$version" --install-location / "$stage/component.pkg"
pkg="$out/nuvira-macos.pkg"
rm -f "$pkg"
if [ -n "${NUVIRA_INSTALLER_IDENTITY:-}" ]; then
    productbuild --package "$stage/component.pkg" --sign "$NUVIRA_INSTALLER_IDENTITY" --timestamp "$pkg"
    pkgutil --check-signature "$pkg"
else
    productbuild --package "$stage/component.pkg" "$pkg"
fi
ls -l "$pkg"

ln -s /Applications "$stage/dmg/Applications"
dmg="$out/nuvira-macos.dmg"
rm -f "$dmg"
hdiutil create -volname "NuviraHub $version" -srcfolder "$stage/dmg" -fs HFS+ -format UDZO -ov "$dmg" >/dev/null
if [ -n "$identity" ]; then
    codesign --force --timestamp --sign "$identity" "$dmg"
fi
ls -l "$dmg"
