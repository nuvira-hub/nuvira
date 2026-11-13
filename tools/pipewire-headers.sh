#!/bin/sh
# Headers of PipeWire 0.3.65 for building on Ubuntu 22.04, whose PipeWire is
# 0.3.48: the Rust bindings need the newer headers, while the program is linked
# against the system's libpipewire-0.3.so.0. A link that succeeds means that
# every PipeWire function the program calls exists in 0.3.48 too, so the same
# binary runs from Ubuntu 22.04 and Linux Mint 21 up (docs/20-linux.md).
#
#   tools/pipewire-headers.sh [DIR]    (default /opt/pw)
#
# Then build with PKG_CONFIG_PATH=DIR/pkgconfig.
set -eu
DIR=${1:-/opt/pw}
VERSION=0.3.65
SRC=$(mktemp -d)
trap 'rm -rf "$SRC"' EXIT
curl -fsSL "https://gitlab.freedesktop.org/pipewire/pipewire/-/archive/$VERSION/pipewire-$VERSION.tar.gz" |
    tar -xz -C "$SRC" --strip-components=1
mkdir -p "$DIR/include/pipewire-0.3/pipewire" "$DIR/include/spa-0.2" "$DIR/pkgconfig"
cp -r "$SRC/spa/include/spa" "$DIR/include/spa-0.2/"
cp -r "$SRC"/src/pipewire/*.h "$SRC/src/pipewire/extensions" "$DIR/include/pipewire-0.3/pipewire/"
sed -e 's/@PIPEWIRE_VERSION_MAJOR@/0/; s/@PIPEWIRE_VERSION_MINOR@/3/; s/@PIPEWIRE_VERSION_MICRO@/65/; s/@PIPEWIRE_API_VERSION@/0.3/' \
    "$SRC/src/pipewire/version.h.in" > "$DIR/include/pipewire-0.3/pipewire/version.h"
cat > "$DIR/pkgconfig/libpipewire-0.3.pc" <<EOF
Name: libpipewire-0.3
Description: PipeWire $VERSION headers, the system's library
Version: $VERSION
Requires: libspa-0.2 >= 0.2
Libs: -lpipewire-0.3
Cflags: -I$DIR/include/pipewire-0.3 -D_REENTRANT
EOF
cat > "$DIR/pkgconfig/libspa-0.2.pc" <<EOF
Name: libspa-0.2
Description: SPA 0.2 of PipeWire $VERSION
Version: 0.2
Cflags: -I$DIR/include/spa-0.2 -D_REENTRANT
EOF
echo "PipeWire $VERSION headers in $DIR (PKG_CONFIG_PATH=$DIR/pkgconfig)"
