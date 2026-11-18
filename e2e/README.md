# End-to-end session tests (loopback)

Characterisation tests of whole NuviraHub sessions with the real components on
127.0.0.1 (full review, Phase 2; `nuvira-pro/docs/review/features.md`, gap 1):
the agent (`serve_reverse`, `serve_direct`, `serve_via_server`) on the synthetic
source, a real `nuvira-server` process built from `../server`, and the bench's
headless viewer, netem proxy and local-server helpers (`../bench`, used as a
library). They pin the **current** behaviour: known bugs are asserted as they are
today (cases named `characterises_bug_...`, comment `KNOWN BUG (Phase 2)` with the
behaviour expected after the fix).

Own workspace (like `bench/`): it builds against the current tree and against the
tag `baseline-pre-review` with this folder and `bench/` copied in.

## Running

```sh
cd e2e
CARGO_TARGET_DIR=<somewhere not on C:> GOCACHE=<go cache> cargo test --release
```

The test binary has its own runner (`harness = false`, `src/runner.rs`): every case
runs in a child process with its own temporary and data folder; output and
`results.json` go to `<target>/release/e2e-runs/<stamp>/` (or `NUVIRA_E2E_OUT`).

| Variable | Effect |
|---|---|
| `NUVIRA_BENCH_SERVER_EXE` | the `nuvira-server` binary; without it the runner builds `../server` with `go build` |
| `NUVIRA_E2E_LONG=1` | also the long cases (8 s / 12 s outages, churn of 60 sessions, idle limit, unanswered confirmation) |
| `NUVIRA_E2E_WILDCARD=1` | on Windows, also the `serve_direct` cases (they bind 0.0.0.0: the firewall shows a prompt for a new program; on Linux they always run) |
| `NUVIRA_E2E_REPEAT=n` | every selected case n times |
| arguments | substring filters of case names (`--exact`, `--list`) |

Xvfb group (Linux): `scripts/run-xvfb.sh` starts a private Xvfb and runs the
`xvfb/` cases with the real X11 capture, XTest input and clipboard of that
virtual display only (file transfers small / 200 MB / cut and resumed, clipboard
both ways, mouse). Needs `xvfb xclip xdotool`.

On a machine whose disk is busy (the NUC's D: during parallel builds: a 4 KiB
`fsync` took 12 s) the server's first SQLite writes can exceed the start timeout:
point `NUVIRA_E2E_OUT` (case data, logs) at a faster disk's temporary folder.

A case that does not run here is printed as `skipped (reason)` and counted; it is
never reported as passed.

## Cases

- `loopback/...`: `serve_reverse` on 127.0.0.1 (one session with a password) and
  `serve_direct`: frames and close, video as datagrams, wrong password, wrong
  certificate, malformed / oversized first message, a 16 MiB message before
  authentication, unauthenticated connections holding the agent, netem N3/N5,
  outages of 1/3 s (8/12 s long), abrupt kill of the viewer or agent process,
  LAN mode without password, an idle connection blocking LAN mode.
- `server/...`: through a local `nuvira-server`: direct path, forced relay,
  WebSocket, impaired relay (`NUVIRA_RELAY_IMPAIR`), wrong temporary password
  without lockout, confirmation accepted / refused / unanswered, permanent password
  with two-factor code (replay, earlier step, lockout), impostor viewer key and
  wrong agent key, an unauthenticated viewer keeping the agent busy, back-to-back
  sessions refused as busy, server restart, viewer killed, three agents and
  viewers at once, shared session with three guests and the fourth refused, idle
  limit fed by input, churn.
- `xvfb/...`: see above.
- `real-desktop/...` (`NUVIRA_E2E_REAL_DESKTOP=1`, only on a test machine whose
  screen, mouse, keyboard and clipboard may be used: `scripts/run-real-desktop.ps1`
  in the signed-in session, e.g. through a scheduled task `/it`): the file cases of
  the Xvfb group with the machine's real capture, and on Windows also the shared
  clipboard both ways (the clipboard text found before is put back) and input
  (`MouseMove` read back with `GetCursorPos`, ShiftLeft down/up seen by
  `GetAsyncKeyState`; no clicks, wheel or characters). On Windows the cases first
  wake the monitor and keep it on for the case: while it sleeps DXGI gives no
  frame at all (pinned in `client/capture/tests/screen.rs`) and the session would
  never get a picture.
