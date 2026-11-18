# Manual entry (docs/review/manual-tests.md): the e2e file cases with the REAL
# screen capture of this Windows machine (the agent binds 0.0.0.0, the firewall
# may ask once for e2e.exe), plus the whole default and long suite.
# Run it in the signed-in user's session of a test machine whose screen may be
# used (not over a plain SSH session: there is no desktop to capture there).
#
#   powershell -ExecutionPolicy Bypass -File run-real-desktop.ps1 -Machine NAME [-Id MT-0xx]
#
# Next to this script: e2e.exe (the test binary, built with
# `cargo test --release --no-run` in e2e/) and nuvira-server.exe (`go build` of
# server/). Results: results\<Id>-<Machine>-<yyyymmdd>.json (+ .log), to copy
# into docs/review/manual-results/.
param(
    [Parameter(Mandatory = $true)][string]$Machine,
    [string]$Id = "MT-E2E-WIN",
    [int]$Repeat = 5
)
$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
$day = Get-Date -Format "yyyyMMdd"
$results = Join-Path $here "results"
New-Item -ItemType Directory -Force $results | Out-Null
$base = Join-Path $results "$Id-$Machine-$day"
$env:NUVIRA_BENCH_SERVER_EXE = Join-Path $here "nuvira-server.exe"
$env:NUVIRA_E2E_OUT = "$base-run"
$env:NUVIRA_E2E_REAL_DESKTOP = "1"
$env:NUVIRA_E2E_WILDCARD = "1"
$env:NUVIRA_E2E_LONG = "1"
$env:NUVIRA_E2E_REPEAT = "$Repeat"
$env:NUVIRA_E2E_RESULT_JSON = "$base.json"
$env:NUVIRA_E2E_RESULT_ID = $Id
$env:NUVIRA_E2E_MACHINE = $Machine
& (Join-Path $here "e2e.exe") *> "$base.log"
$code = $LASTEXITCODE
Write-Host "exit $code; results in $base.json"
exit $code
