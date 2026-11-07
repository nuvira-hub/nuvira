#!/bin/sh
# After installing or updating the .deb or .rpm: the NuviraHub printer (remote
# printing), when CUPS is installed and running. Without it everything else works.
if command -v lpstat >/dev/null 2>&1; then
    /usr/bin/nuvira service printer || echo "NuviraHub printer not installed: remote printing is not available"
fi
exit 0
