#!/bin/sh
# Before removing the .deb or .rpm (not before an update): the NuviraHub printer,
# the sign-in screen's agent and the timer of the automatic updates. Installed for
# the whole computer, the computer keeps its ID in /var/lib/nuvira for a later
# installation (delete the folder to forget it).
# dpkg passes "remove" or "purge"; rpm passes the number of versions left (0).
case "$1" in
    remove | purge | 0)
        if [ -e /etc/nuvira/managed.json ]; then
            /usr/bin/nuvira service deploy --remove || true
        else
            /usr/bin/nuvira service system --remove >/dev/null 2>&1 || true
        fi
        /usr/bin/nuvira service printer --remove || true
        if [ -d /run/systemd/system ]; then
            systemctl disable --now nuvira-update.timer >/dev/null 2>&1 || true
        fi
        ;;
esac
exit 0
