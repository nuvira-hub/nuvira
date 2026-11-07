# NuviraHub — Remote restart

[← Index](README.md)

During a session the technician can restart the controlled computer; the viewer window stays open, waits for the computer to come back and connects again by itself. Typical cases: an installation or an update that wants a restart, a computer that has become slow, a driver to load.

- **From the viewer.** The toolbar button with the power symbol, after a confirmation dialog: open programs on the remote computer are closed without asking, unsaved work there is lost. The button appears only when the agent offers the restart. `nuvira-viewer --restart` asks for it as soon as the session starts, without the dialog (automated tests).
- **Waiting.** The agent answers before restarting. The viewer shows "The remote computer is restarting", then, once the connection drops, a message in the middle of the window ("Waiting for … to restart") and the title "waiting for … to restart". The first attempt comes after 20 s (the computer takes a while to go down), the next ones every 5 s, for at most 10 minutes. When the session starts again the viewer says so and everything goes on as at the start: video, toolbar, open terminal windows (with a new shell) and TCP tunnels (listening again on the same local ports).
- **When it cannot reconnect by itself.** Only the permanent password works after a restart without anyone at the computer. With a temporary password or a support ticket, or when the server's rules require confirmation also with the permanent password, the agent says so in its answer (`reconnect: false`): the computer restarts, the viewer closes with "… is restarting. Connect again once it is back, with the password shown on it then." Also the viewer gives up when the agent refuses the access (wrong password, not accepted) or after 10 minutes, with the reason.

## Where it is offered

Only with unattended access, which brings the agent back by itself after the boot (`--ipc`: the agent started by the service, `ServerOptions::unattended`); not in view-only mode, with a synthetic image or in a terminal-only session.

| System | How | Notes |
|---|---|---|
| Windows | `shutdown.exe /r /f /t 3 /d p:4:1` from the agent (SYSTEM) | The service starts the agent again at the boot, also before anyone signs in (lock screen). `/f` closes the programs without asking; 3 s let the answer reach the viewer |
| Linux | `systemctl reboot --no-ask-password` as the signed-in user (logind) | logind allows it to the user of the active local session when no other user is signed in; otherwise it is refused and the viewer shows the reason. The computer comes back only with automatic login (see [20-linux.md](20-linux.md)) |
| macOS | Apple event "restart" (`kAERestart`) to loginwindow through `osascript`, 2 s after the answer | No administrator needed. The apps are asked to quit: one with unsaved work can stop the restart (the agent then accepts sessions again after 3 minutes, and the waiting viewer simply connects again). The Mac comes back only with automatic login, then the LaunchAgent starts the agent (see [33-macos.md](33-macos.md)) |

## Protocol

- `Hello.restart` (agent): it can restart the computer and comes back by itself.
- `Restart {}` (viewer → agent, control channel).
- `RestartState { ok, error, reconnect }` (agent → viewer): `error` is a reason ("@key" translated, or the system's message); `reconnect` true when the session was opened with the permanent password and no confirmation.
- After a successful `Restart` the agent ends the session and refuses new ones with `@agent.restarting` until it ends: a viewer that reconnects too early is told so and keeps waiting instead of finding the computer still on its way down. The viewer keeps trying after a connection that fails or a refusal `@agent.restarting`, and stops at any other refusal (`AuthResult` not ok).

See `proto/nuvira/v1/session.proto`, `client/agent/src/restart.rs` and `App::wait_for_restart` in `client/viewer/src/lib.rs`.

## Tests

2026-11-06, viewer on the NUC with `--restart` through the NAS server (`alexbn.ddns.net`), permanent password, agents started by the service:

| Controlled computer | Restart accepted | Computer booted | Viewer connected again | Total |
|---|---|---|---|---|
| PC-LABORATORIO, Windows 10 22H2 (service as SYSTEM) | 20:48:14 | 20:48:44 | 20:49:06 | 52 s |
| VM Mint 22.3, Cinnamon X11 (user service, automatic login) | 21:16:31 | 21:16:42 | 21:17:03 | 32 s |
| VM Fedora 44, GNOME Wayland (user service, automatic login) | 21:19:36 | 21:19:48 | 21:20:09 | 33 s |

In every case: one attempt after 20 s, then "device not reachable" every 5 s until the agent was back, then the session went on (video, decoder started again) until the end of the test. Windows logged event 1074 with the comment "NuviraHub: remote restart" by `NT AUTHORITY\SYSTEM`. On Fedora the GNOME screen permission was restored without a dialog (`restored=true`) and PipeWire capture started again.

Toolbar button, same day, PC-LABORATORIO, viewer in Italian: the button "Riavvia il computer remoto" (pressed through UI Automation), the confirmation dialog with the computer's name and the warning, "Sì": restart at 21:26:27, connected again at 21:27:09 (42 s), the button back in the toolbar.

Temporary password, same day, VM Fedora: a script on the agent's Unix socket in place of the app (reads the temporary password, accepts the request); viewer with that password and `--restart`: confirmation pending → connected → `RestartState { ok: true, reconnect: false }` at 21:29:57 → the viewer closed at once with "VM-Fedora-NuviraHub si sta riavviando. Ricollegati quando è di nuovo acceso, con la password che mostrerà allora." (`@closed` for the app); Fedora booted at 21:30:02 with the service active.

Not tested yet: a Linux computer with another user signed in (logind refuses), a refusal `@agent.restarting` (the computers went down before the first attempt).
