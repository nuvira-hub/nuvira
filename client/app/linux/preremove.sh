#!/bin/sh
# Before removing the .deb or .rpm (not before an update): the NuviraHub printer.
# dpkg passes "remove" or "purge"; rpm passes the number of versions left (0).
case "$1" in
    remove | purge | 0)
        /usr/bin/nuvira service printer --remove || true
        ;;
esac
exit 0
