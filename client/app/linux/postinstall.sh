#!/bin/sh
# After installing or updating the .deb or .rpm:
# - the NuviraHub printer (remote printing), when CUPS is installed and running;
# - the timer of the automatic updates (root's: the program is in /usr/bin);
# - the sign-in screen's agent, when unattended access installed it, starts again
#   with the new program;
# - the installation for the whole computer (`nuvira service deploy`), when the
#   organisation's policy file is there (put before the package, e.g. by Ansible)
#   or it was already installed: (again) with this program.
# Without them everything else works.
if command -v lpstat >/dev/null 2>&1; then
    /usr/bin/nuvira service printer || echo "NuviraHub printer not installed: remote printing is not available"
fi
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
    # The timer only: the update in progress may be the one installing this.
    systemctl enable nuvira-update.timer >/dev/null 2>&1 || true
    systemctl start nuvira-update.timer || true
    if [ -e /etc/nuvira/policy.json ] || [ -e /etc/nuvira/managed.json ]; then
        /usr/bin/nuvira service deploy || echo "NuviraHub: the installation for the whole computer failed (see above)"
    else
        systemctl try-restart nuvira-signin.service >/dev/null 2>&1 || true
    fi
fi
exit 0
