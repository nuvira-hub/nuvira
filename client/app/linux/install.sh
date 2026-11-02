#!/bin/sh
# Installs NuviraHub for the current user, in ~/.local (no sudo needed).
# Run it again to update; run it with --uninstall to remove it.
set -e

here=$(cd "$(dirname "$0")" && pwd)
data=${XDG_DATA_HOME:-$HOME/.local/share}
prefix=$data/nuvira
bin=$HOME/.local/bin
apps=$data/applications
icons=$data/icons/hicolor
app_id=io.github.nuvira-hub.nuvira

if [ "$1" = "--uninstall" ]; then
    rm -rf "$prefix"
    rm -f "$bin/nuvira" "$apps/$app_id.desktop" "$apps/nuvira-viewer.desktop"
    for size in 32 128 256; do
        rm -f "$icons/${size}x${size}/apps/$app_id.png" "$icons/${size}x${size}/apps/nuvira-viewer.png"
    done
    # An icon cache there (written by older versions of this script) would
    # still list the icons just removed.
    if [ -f "$icons/icon-theme.cache" ] && command -v gtk-update-icon-cache >/dev/null; then
        gtk-update-icon-cache -q -f -t "$icons" || true
    fi
    echo "NuviraHub removed. Your ID and address book are still in ${XDG_CONFIG_HOME:-$HOME/.config}/nuvira, if you want to delete them too."
    exit 0
fi

mkdir -p "$prefix" "$bin" "$apps"
install -m 755 "$here/nuvira" "$prefix/nuvira"
ln -sf "$prefix/nuvira" "$bin/nuvira"

for size in 32 128 256; do
    mkdir -p "$icons/${size}x${size}/apps"
    install -m 644 "$here/icons/app-$size.png" "$icons/${size}x${size}/apps/$app_id.png"
    install -m 644 "$here/icons/viewer-$size.png" "$icons/${size}x${size}/apps/nuvira-viewer.png"
done

# Absolute paths: GNOME keeps stale icons of a name in its cache, and
# ~/.local/bin is not always in the PATH of the desktop session.
sed -e "s|^Exec=.*|Exec=$prefix/nuvira|" \
    -e "s|^Icon=.*|Icon=$icons/256x256/apps/$app_id.png|" \
    "$here/$app_id.desktop" > "$apps/$app_id.desktop"
sed -e "s|^Exec=.*|Exec=$prefix/nuvira|" \
    -e "s|^Icon=.*|Icon=$icons/256x256/apps/nuvira-viewer.png|" \
    "$here/nuvira-viewer.desktop" > "$apps/nuvira-viewer.desktop"

# No icon cache of our own in ~/.local: once written it must be rebuilt at every
# change, or GTK keeps looking for icons that are no longer there.
command -v update-desktop-database >/dev/null && update-desktop-database -q "$apps" || true

echo "NuviraHub installed: open it from the applications menu, or run: $prefix/nuvira"
