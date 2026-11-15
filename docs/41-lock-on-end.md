# NuviraHub — Lock the computer when the session ends

[← Index](README.md)

When a session ends the controlled computer can be locked, as with Win+L: whoever
uses it next has to sign in. Three ways to want it, any one is enough:

| Who | Where | Can the technician turn it off? |
|---|---|---|
| The person at the computer | App, Settings, "Lock this computer when a session ends" (`permissions.json` of the agent, `AgentCommand::LockOnEnd`) | No |
| The server's administrator (NuviraHub Pro) | Rule "Lock the computer when the session ends" (`Policy.lock_on_end`), also as a limit of an account, a member of a book or the sessions without an account | No |
| The technician, for one session | Lock button in the session bar, or `--lock-on-end` | Yes, the same button |

Not locked: when the session is handed over to a guest (the guest carries on and
the lock comes at the end of its session), when the computer is restarting, at the
end of a guest's own part, and on the sign-in screen (nobody to lock).

## Protocol

- `Hello.lock_on_end` (agent): it can lock (a person signed in, through a server).
- `SetLockOnEnd { on }` (viewer → agent): the technician's choice for this session.
- `LockOnEndState { on, forced }` (agent → viewer): sent after the first
  `Permissions` (the viewer reads `VideoFormat` right after `AuthResult`) and after
  every `SetLockOnEnd`; `forced` when the computer's setting or the rules want it.
  The bar's button is then on and disabled.
- `Policy.lock_on_end` (field 14): rule of the server, merged with the session's
  limits like the others (it can only add).
- `AgentEvent::Rules.lock_required`: the app shows its setting ticked and locked.

Older agents ignore `SetLockOnEnd` and send no state: the button stays hidden.
An older service agent ignores the app's `LockOnEnd` command (a warning in its log,
the connection carries on).

## How each system locks (`client/agent/src/lock.rs`)

- **Windows**: `LockWorkStation`. It works also from the agent the service starts
  as SYSTEM, which runs on the interactive desktop of the person's session. If it
  fails, `WTSDisconnectSession` on the agent's own session (the console shows the
  lock screen too).
- **Linux**: `loginctl lock-session` on the person's graphical session
  (`loginctl show-user UID -p Display`): the agent runs as a systemd user service,
  outside any session, so `lock-session` without an ID would not know which one.
  GNOME and KDE Plasma lock inside the shell. Cinnamon locks through its
  screensaver daemon: when that daemon is not running, logind's request reaches
  nobody, so the agent then runs `cinnamon-screensaver-command --lock`, which
  starts it through D-Bus and locks.
- **macOS**: `SACLockScreenImmediate` of the private login framework (what the
  menu bar's "Lock Screen" does). Not tried yet.

## Tests of 2026-11-15

Viewer on PC-LABORATORIO or on the Mint VM, through the NAS server or the real one.

| Controlled computer | How | Result |
|---|---|---|
| PC-LABORATORIO, agent of the person (test agent) | `--lock-on-end` from the Mint viewer, permanent password | Locked (`LogonUI.exe` running), state `on=true forced=false` in the viewer |
| PC-LABORATORIO, agent of the installed service (SYSTEM) | Same, through alexbn.ddns.net | Locked by `LockWorkStation` (no fallback), the session stays "Active" |
| PC-LABORATORIO, service, `lock_on_end` in `permissions.json` | Viewer without the option | `on=true forced=true` in the viewer, locked |
| PC-LABORATORIO, the app of this build with the service | `lock_on_end` in `app.json`: the app sends it when it connects; then the checkbox unticked through UI Automation | The service's agent logged `on=true`, then `on=false`, and saved it; `app.json` updated |
| Mint 22.3, Cinnamon X11 | `--lock-on-end` from PC-LABORATORIO | Locked. On the VM the screensaver daemon was not running (it ends 20–30 s after being unlocked with `cinnamon-screensaver-command -d`, with Python errors): the fallback started it and locked |
| Ubuntu 26.04, GNOME Wayland | Same | Locked (`LockedHint=yes`) |
| Kubuntu 26.04, KDE Plasma Wayland | Same | Locked (`LockedHint=yes`) |

- Pro rule end to end: the rule set in the web console of the Pro test server
  (Ubuntu VM), the test agent on the Mint registered there without a setting of
  its own: the viewer got `on=true forced=true`, the Mint was locked when the
  session ended.

Still to try: macOS.
