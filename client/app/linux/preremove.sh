#!/bin/sh
# Before removing the .deb or .rpm (not before an update): the NuviraHub printer
# and the timer of the automatic updates.
# dpkg passes "remove" or "purge"; rpm passes the number of versions left (0).
case "$1" in
    remove | purge | 0)
        /usr/bin/nuvira service printer --remove || true
        if [ -d /run/systemd/system ]; then
            systemctl disable --now nuvira-update.timer >/dev/null 2>&1 || true
        fi
        ;;
esac
exit 0
