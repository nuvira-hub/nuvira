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
</dict>
</plist>
EOF
cp LICENSE "$app/Contents/Resources/"

# The virtual HID keyboard that types into the lock screen (docs/33-macos.md,
# "Locked screen"): Karabiner-DriverKit-VirtualHIDDevice, public domain, signed and
# notarized by its author; installed by `nuvira service system`.
KVHID_VERSION=8.7.0
KVHID_SHA256=3364294fca2629d4d8968bbc434537f4093b8ba620a211ec192b3d2e71e42dfb
kvhid="$app/Contents/Resources/Karabiner-DriverKit-VirtualHIDDevice.pkg"
curl -fsSL --retry 3 -o "$kvhid" \
    "https://github.com/pqrs-org/Karabiner-DriverKit-VirtualHIDDevice/releases/download/v$KVHID_VERSION/Karabiner-DriverKit-VirtualHIDDevice-$KVHID_VERSION.pkg"
echo "$KVHID_SHA256  $kvhid" | shasum -a 256 -c -

if [ -n "$identity" ]; then
    # Nested code first: the app's signature seals it.
    codesign --force --options runtime --timestamp --sign "$identity" "$app/Contents/MacOS/nuvira-cups"
    codesign --force --options runtime --timestamp --sign "$identity" "$app"
    codesign --verify --strict --verbose=2 "$app"
fi

# The signed app alone, for the automatic updates (channel "macos", signed by
# tools/sign-unix-updates.sh); ditto keeps the bundle's signature intact.
ditto -c -k --keepParent "$app" "$out/nuvira-macos-app.zip"

ln -s /Applications "$stage/dmg/Applications"
dmg="$out/nuvira-macos.dmg"
rm -f "$dmg"
hdiutil create -volname "NuviraHub $version" -srcfolder "$stage/dmg" -fs HFS+ -format UDZO -ov "$dmg" >/dev/null
if [ -n "$identity" ]; then
    codesign --force --timestamp --sign "$identity" "$dmg"
fi
ls -l "$dmg"
