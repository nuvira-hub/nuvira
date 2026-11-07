#!/bin/bash
# Builds the Linux packages: nuvira-linux-x64.tar.gz (the app, its icons, the
# .desktop files and install.sh), nuvira-linux-x64.deb and nuvira-linux-x64.rpm
# (with nfpm). The names carry no version, for the "latest release" links of the
# website. Build on the oldest distribution we support (Debian 12): the
# binary needs a glibc at least as new as the build one.
#
#   tools/package-linux.sh [out_dir]
set -euo pipefail
cd "$(dirname "$0")/.."

out=${1:-dist}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
stage=$(mktemp -d)/nuvira-$version
trap 'rm -rf "$(dirname "$stage")"' EXIT

cargo build --release -p nuvira-app -p nuvira-cups

mkdir -p "$stage/icons" "$out"
out=$(cd "$out" && pwd)
install -m 755 target/release/nuvira "$stage/nuvira"
strip "$stage/nuvira"
# The CUPS backend of the NuviraHub printer (remote printing).
install -m 755 target/release/nuvira-cups "$stage/nuvira-cups"
strip "$stage/nuvira-cups"
install -m 755 client/app/linux/install.sh "$stage/install.sh"
cp client/app/linux/*.desktop "$stage/"
cp client/app/icons/32x32.png "$stage/icons/app-32.png"
cp client/app/icons/128x128.png "$stage/icons/app-128.png"
cp client/app/icons/icon.png "$stage/icons/app-256.png"
cp client/viewer/icons/viewer-32.png "$stage/icons/viewer-32.png"
cp client/viewer/icons/viewer-256.png "$stage/icons/viewer-256.png"
# There is no 128 px viewer icon: the 256 one is fine at that size.
cp client/viewer/icons/viewer-256.png "$stage/icons/viewer-128.png"
cp LICENSE "$stage/"
cat > "$stage/README.txt" <<EOF
NuviraHub $version for Linux (x86-64)

On Linux, for now, NuviraHub controls other computers: this computer
cannot be controlled remotely yet.

Install for the current user (no sudo):   ./install.sh
Remove:                                   ./install.sh --uninstall

It needs these packages (on Ubuntu/Debian; most desktops have them):
  libwebkit2gtk-4.1-0 libgtk-3-0 libxkbcommon-x11-0 libpipewire-0.3-0

It is built on Debian 12, so it needs glibc 2.36 and PipeWire 0.3.65 or
later: Debian 12, Ubuntu 24.04, Linux Mint 22, Fedora 40 or later. It
controls other computers with X11 and with Wayland; this computer can be
controlled with a Wayland session.
EOF

tar -C "$(dirname "$stage")" -czf "$out/nuvira-linux-x64.tar.gz" "nuvira-$version"
cp client/app/linux/nfpm.yaml client/app/linux/postinstall.sh client/app/linux/preremove.sh "$stage/"
for packager in deb rpm; do
    (cd "$stage" && VERSION=$version nfpm package -f nfpm.yaml -p $packager -t "$out/nuvira-linux-x64.$packager")
done
ls -l "$out"
