# NuviraHub — Remote terminal

[← Index](README.md)

A command prompt of the remote computer in a window of the viewer: Windows PowerShell on Windows, the user's login shell on Linux and macOS. It needs no desktop: a terminal-only session captures nothing and sends no video.

- **Terminal only.** In the app, "Terminal only" under Connect, or "Terminal" next to a computer of the address book; `nuvira viewer --terminal` from the command line. The window shows the connection (and the confirmation at the PC, when asked) and then the shell; closing the window ends the session.
- **In a desktop session.** The toolbar button with a prompt opens a terminal window next to the session; several can be open, each with its own shell.
- **Keys.** Everything typed goes to the shell (Ctrl+C is the interrupt); Ctrl+Shift+C copies the selection, Ctrl+Shift+V pastes. The window follows its size (columns and rows go to the shell).
- **End.** When the shell ends (`exit`) the window says so with the exit code; closing the window ends the shell and everything it started; so does the end of the session.

## Who the shell runs as

- **Windows.** The shell has the rights of the user signed in on the PC, also when the agent is the service's (SYSTEM): the service's agent starts it with that user's token (`WTSQueryUserToken`), environment and profile folder, at the normal (medium) level, without elevation. With nobody signed in there is no terminal (`@agent.terminal_no_user`). Administrator work goes through a desktop session (UAC prompt). Decision of 2026-11-06.
- **Linux and macOS.** The agent runs as the user: the shell is that user's (`$SHELL -l`, else `/bin/sh`), in its home folder, with `TERM=xterm-256color`.

## Who can forbid it

- **The person at the PC.** App settings → "This computer" → "Allow the remote terminal" (on by default). Kept like the privacy mode setting: `app.json` (`no_terminal`) and the agent's `permissions.json` (`terminal`), so it holds while the app is closed.
- **The administrator of a Pro server.** Rule "No remote terminal" (web console, Rules; app, Administration), `Policy.no_terminal`.
- When forbidden, or in view-only mode, the agent does not announce it (`Hello.terminal`): the button does not appear and a terminal-only session ends with "the remote computer does not offer the terminal". Each terminal opened is checked again.
- **Seen at the PC.** Nothing shows on the screen during a terminal-only session: the strip of the app says "… has a terminal open on this computer" (`SessionStarted.terminal`). Connection requests with the temporary password are confirmed as usual.

## How it works

- **Protocol** (`proto/nuvira/v1/session.proto`). `Hello.terminal` from the agent, `Hello.terminal_only` from the viewer (the agent then answers only ping and chat on the control channel). Every bidirectional stream the viewer opens after the control one starts with `StreamOpen` (terminal, with the size, or a TCP tunnel); a terminal then carries `TerminalMessage`: `data` both ways, `resize`, and `exit` (code, or the reason it did not start) from the agent.
- **Agent** (`client/agent/src/pty.rs`, `terminal.rs`). Windows: ConPTY (`CreatePseudoConsole`), Windows PowerShell 5 (`-NoLogo`), `CreateProcessAsUserW` with the user's token or `CreateProcessW` as the agent's own user; empty standard handles (`STARTF_USESTDHANDLES`), otherwise with the agent's output redirected to a file the shell wrote there. Linux/macOS: `openpty`, a session of its own with the terminal as the controlling one; it gets SIGHUP and SIGKILL (the whole group) at the end. A thread reads the output (16 KB pieces, back-pressure through a channel of 16), one writes the keys, one waits for the exit code.
- **Viewer** (`client/viewer/src/terminal.rs`, `terminal.html`). xterm.js 5.5 with the fit addon, embedded (`client/viewer/src/xterm/`, MIT licence, source and hashes in its README): no file is loaded from the network. The page sends the keys as text and the size in characters; the output arrives base64-encoded.

## Tests

2026-11-06, with automated typing (`--terminal-type`, `--terminal-log`, hidden options for tests):

| Controlled computer | Viewer | Result |
|---|---|---|
| PC-LABORATORIO, Windows 10, agent as the user | NUC, terminal only | `whoami` = pc-laboratorio\mrbla, PowerShell 5, `exit 7` → code 7 |
| PC-LABORATORIO, the service (SYSTEM), through the owner's Pro server | NUC, terminal only, request accepted at the PC | `whoami` = pc-laboratorio\mrbla, folder C:\Users\mrbla, level medium (S-1-16-8192); the app got `terminal: true` |
| Ubuntu 26.04 VM (Wayland) | NUC, terminal only | bash login shell, 106 columns, TERM=xterm-256color, `exit 3` → code 3 |
| PC-LABORATORIO | Ubuntu VM (WebKitGTK window), terminal only | `whoami`, computer name, `exit 5` → code 5 |
| Ubuntu VM, desktop session | PC-LABORATORIO, toolbar button clicked through UI Automation | terminal opened next to the session, command run, `exit` → code 0 |
| Ubuntu VM | PC-LABORATORIO, keys typed for real (SendKeys) and window made smaller | the keys reached bash; columns 113 → 72 |
| PC-LABORATORIO with "Allow the remote terminal" off, then Pro rule "No remote terminal" on | NUC | both times "the remote computer does not offer the terminal" |
| PC-LABORATORIO, app with the service | — | the setting unticked and ticked through UI Automation: the service's agent logged it and wrote `permissions.json` |

Nobody at the console, 2026-11-10: PC-LABORATORIO with the service of 0.22.0, mrbla's session disconnected (`tsdiscon`), so the service's agent on the sign-in screen of the console; viewer on the Mint VM with `--terminal`: the terminal opened and ended at once with code 0 and "nessun utente collegato al computer remoto" (`@agent.terminal_no_user`), as decided. A session left disconnected counts as nobody: the shell is started only with the user of the session the agent is in.

Not tested yet: macOS, the "Terminal only" and address book buttons of the app (the same viewer option as the tests), very large outputs.
