# NexoDesk — Phase 3: system service (unattended)

[← Index](README.md)

## Why

An agent started by the user cannot see Windows' **secure desktop**: administrator prompts (UAC), the lock screen, Ctrl+Alt+Del. During the test of 2026-10-27 the FileZilla update froze the image until someone answered at the PC (see [08-roadmap.md](08-roadmap.md)). The service solves this and allows access to a PC with nobody in front of it.

## How it works (Windows)

- The service is `nexodesk.exe service run`: the app contains agent, viewer and service (one program to download, copy and sign). It runs as **LocalSystem** with automatic start, in session 0, which has no screen. `nexodesk-service.exe` and `nexodesk-agent.exe` are still built for development and tests.
- Every half second it reads the console session (`WTSGetActiveConsoleSessionId`) and keeps the agent (`nexodesk.exe agent --ipc ...`) running in that session:
  - the agent starts with a copy of the service's SYSTEM token moved into the session (`SetTokenInformation(TokenSessionId)` + `CreateProcessAsUserW`, desktop `winsta0\default`);
  - at every session change (disconnection, user switch) the agent is closed and restarted in the new one;
  - if the agent exits by itself it is restarted, with a growing delay up to 30 s if it keeps failing right away;
  - the agent runs in a job object with `KILL_ON_JOB_CLOSE`: if the service dies, Windows closes the agent too (no orphan copies with the same ID).
- **Secure desktop**: the capture and input threads follow the desktop that receives input (`OpenInputDesktop` + `SetThreadDesktop`, in `nexodesk-capture/src/desktop.rs`). When DXGI duplication stops because of a desktop switch, it is recreated on the new one. Input goes through a dedicated thread that checks the desktop before every event. Only a SYSTEM process can open `Winlogon`: as a user the switch fails and capture resumes when the normal desktop comes back.
- As SYSTEM, input also reaches windows opened as administrator (no UIPI block).

## Files

| Path | Content | Access |
|---|---|---|
| `%ProgramFiles%\NexoDesk\` | `nexodesk.exe` | writable by administrators only (the service must not run files a user can replace) |
| `%ProgramData%\NexoDesk\service.json` | server, permanent password, transport | SYSTEM and administrators only |
| `%ProgramData%\NexoDesk\device.key` | device key: the ID stays the same even after reinstalling | same |
| `%ProgramData%\NexoDesk\online` | "ID password" written by the agent once registered | same |
| `%ProgramData%\NexoDesk\service.log`, `agent.log` | logs (rotated at 5 and 10 MB) | same |

The password is passed to the agent in the `NEXODESK_PASSWORD` variable, not on the command line, and is never written to the logs.

## Usage

From an administrator command prompt:

```text
nexodesk service install --server nexodesk.example.com [--password ...] [--server-key ...]
nexodesk service status
nexodesk service uninstall
```

`install` copies `nexodesk.exe` into Program Files (whatever the file it runs from is called; from `nexodesk-service.exe` it takes the `nexodesk.exe` next to it; an earlier installation is updated: service command changed, old programs removed), protects the data folder, registers the service (automatic restart if it fails), adds the "NexoDesk agent" firewall rule and prints ID and password. Run again on an existing service it updates it; without `--password` it keeps the one already set. `uninstall` leaves programs and data, so reinstalling keeps the same ID.

## Test of 2026-10-27

Service installed on PC-LABORATORIO, viewer on the NUC:

- the agent runs as `NT AUTHORITY\SYSTEM` in the console session;
- registration on the server 4 s after the service starts;
- UAC prompt opened during the session: capture switches to the `Winlogon` desktop immediately and the viewer shows the "Do you want to allow this app..." window. Once the prompt is closed, it goes back to `Default` within a few milliseconds, without interrupting the session;
- median latency 24 ms in a LAN;
- lock screen: with the session open from the NUC, LABORATORIO was locked; the lock screen appeared in the viewer and the user unlocked it remotely with the PIN. Capture and input moved to `Winlogon` and back to `Default` after unlocking;
- restart: LABORATORIO restarted at 08:11:18, service started at 08:11:33, agent registered at 08:11:39 with no user logged in. From the NUC the sign-in screen is visible; the user entered the PIN remotely and the session continued on the desktop (signing in does not change the console session);
- shutdown: Windows closes the sessions (and the agent, code `0x40010004`) before stopping services. The service treated this as a crash and scheduled a restart; now it recognises it and restarts silently only if the session continues.

## Ctrl+Alt+Del

Ctrl+Alt+Del cannot be sent as keys: the viewer's own system always intercepts it. The viewer uses **Ctrl+Alt+End** (as in Remote Desktop) and the toolbar button ([15-toolbar.md](15-toolbar.md)).

- The viewer sends `InputEvent.secure_attention` (`session.proto`).
- Windows runs `SendSAS` only when a **service** calls it (the agent, even though it is SYSTEM, is silently ignored) and when the `SoftwareSASGeneration` policy (`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System`) allows it for services. `install` adds the value 1 without removing those already present.
- The agent signals the `Global\NexoDeskSAS` event, created by the service: with the default security of an object created by SYSTEM, only SYSTEM can signal it. The service runs `SendSAS(FALSE)`.
- Without the service (agent started by the app) Ctrl+Alt+Del is not available and the agent writes so in the log.

Test of 2026-10-27: from the NUC, Ctrl+Alt+End brings up the security screen on LABORATORIO in about 240 ms; Esc goes back to the desktop.

## App and service on the same PC

With the service installed the app does not start its own agent: it shows the ID and password of the service's agent and handles its confirmation requests.

**Two passwords:**

| Password | Who sees it | Confirmation by the person at the PC |
|---|---|---|
| Permanent (`install --password`, in `service.json`) | only the administrator who installs | no: unattended access |
| Temporary (generated by the agent, "New" in the app) | the person using the PC, in the app | yes, within 30 s; with no app open the request is refused immediately |

Confirmation comes **after** the password check: the person at the PC is not bothered by someone who gets the password wrong, and the agent knows which of the two was used. This also applies to the app without the service (temporary password only, always with confirmation). The command-line agent without `--ipc` asks for no confirmation, because there is nobody to ask.

**Local channel** (`client/agent/src/ipc.rs`):

- the service's agent (started with `--ipc`) creates the named pipe `\\.\pipe\nexodesk-agent-<Windows session>`, accessible to SYSTEM and interactive users (SDDL `D:P(A;;GA;;;SY)(A;;GRGW;;;IU)`); it accepts only apps from its own session (`GetNamedPipeClientSessionId`);
- the first instance is created only if the name is free (`FILE_FLAG_FIRST_PIPE_INSTANCE`); then the agent always prepares the next instance before using the connected one, so the name stays its own even when the app disconnects;
- one JSON line per message: to the app `{"type":"info","server":...}` and `{"type":"event","event":{...}}` (the `AgentEvent`s), to the agent the `AgentCommand`s (`respond`, `end_session`, `new_password`, and the clipboard, chat and file commands);
- one app at a time; as soon as it connects it receives the current state (registration, password, session and pending request);
- the app recognises the service through the service manager (readable by a normal user too) and, if its agent does not answer (start-up, user switch, update), retries every 2 s.

Tests of 2026-10-27 on PC-LABORATORIO, viewer from the NUC:

- the app shows the service's ID, "Temporary password" and the note about unattended access;
- permanent password: access with no request;
- temporary password: the request appears in the app, "Accept" starts the session;
- temporary password with the app closed: refused immediately, the viewer shows "connection not accepted on the device";
- the app reconnects after an agent update and after two closes in a row. The first version did not recreate the pipe after the app closed ("Access denied" with `FIRST_PIPE_INSTANCE`): fixed by preparing the next instance in advance.

## To do

- Check on the local channel that the other end really is the service's agent (a SYSTEM process in Program Files) and not a program that took the pipe name before it.
- In the viewer, a "waiting for confirmation" message while the person at the PC decides (today the window stays on "connecting…").
- Remote Desktop (RDP) sessions: today only the console is followed.
- Permanent password stored as a hash instead of in clear text (the check in the agent must change too).
