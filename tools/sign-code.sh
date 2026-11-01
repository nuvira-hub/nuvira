#!/usr/bin/env bash
# Signs Windows executables with the code signing certificate (Authenticode), so
# that Windows and antivirus software see who published them: unsigned, an
# antivirus took the installation of the service for malware (Kaspersky,
# PDM:Trojan.Win32.Generic, 2026-11-01).
#
#   tools/sign-code.sh FILE...
#
# The certificate is Certum's Open Source Code Signing in the cloud: SimplySign
# Desktop must be connected (code from the SimplySign app), then the certificate is
# in the user's store. Files already signed are left as they are, so that the same
# nuvira.exe goes into the setup, the update and the release.
#
#   NUVIRA_SIGN_SUBJECT   name on the certificate (default "Alessandro Blason")
#   NUVIRA_UNSIGNED=1     skip signing (test builds only, never for a release)

set -euo pipefail

SUBJECT="${NUVIRA_SIGN_SUBJECT:-Alessandro Blason}"
TIMESTAMP="http://time.certum.pl"

if [ "${NUVIRA_UNSIGNED:-}" = 1 ]; then
    echo "== NOT signing (NUVIRA_UNSIGNED=1): $*"
    exit 0
fi

# Git Bash would turn /n, /fd... into paths.
export MSYS2_ARG_CONV_EXCL='*'

SIGNTOOL="$(find "/c/Program Files (x86)/Windows Kits/10/bin" -path '*/x64/signtool.exe' 2>/dev/null | sort -V | tail -1)"
if [ -z "$SIGNTOOL" ]; then
    echo "signtool.exe not found: install the Windows SDK" >&2
    exit 1
fi

for file in "$@"; do
    if "$SIGNTOOL" verify /pa /q "$(cygpath -w "$file")" >/dev/null 2>&1; then
        echo "== already signed: $file"
        continue
    fi
    echo "== signing $file"
    "$SIGNTOOL" sign /n "$SUBJECT" /fd sha256 /tr "$TIMESTAMP" /td sha256 /q "$(cygpath -w "$file")"
    "$SIGNTOOL" verify /pa /q "$(cygpath -w "$file")" >/dev/null
done
