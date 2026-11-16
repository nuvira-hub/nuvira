# Switch sides

[← Index](README.md)

During a session the technician can show their own screen to the person at the controlled computer, who can then see it and use it: to show how something is done, or so that a colleague who called for help takes control of the technician's computer. Nothing is typed again: no ID, no password, no new connection through the server.

## How it looks

- **Technician**: the toolbar's "Switch sides: show your screen" button (shown when the agent offers it and this program can serve a screen). The session window goes to the taskbar (the other computer would otherwise see it showing its own screen), and the notice says "PC-B now sees your screen and can use it…". The same button, now "Take control back", ends it; the window comes back with "You took control back".
- **At the controlled computer**: a window opens for the person signed in, titled "PC-A's screen — NuviraHub", with the technician's screen; they can use the mouse and keyboard there. Closing it ends the switch, and the technician's window comes back with "PC-B closed the window with your screen".
- What the person at the controlled computer gets on the technician's computer: the picture, the keyboard and the mouse only. No files, clipboard, terminal, tunnels, printing, recording, privacy mode, restart, resolution or sound (`reverse_policy`).
- When the session ends, the window closes with it.

## How

Inside the session already open, as a QUIC connection within a stream (`client/transport/src/nested.rs`):

- **Technician (viewer)**: `ReverseHosting` opens a bidirectional stream with `StreamOpen.reverse` = `ReverseOpen { password, fingerprint, name }` (a new self-signed certificate, `server_config`; a random password), then serves on that stream an agent's endpoint whose socket is the stream (`StreamSocket`: each QUIC datagram as a big-endian u16 length and its bytes; a stream and not QUIC datagrams, because QUIC needs at least 1200 bytes per datagram, more than the outer connection's datagrams can carry). The session served is the agent's own `session()` (`nuvira_agent::serve_reverse`, `Reverse { password }`), handed to the viewer by the app (`nuvira_viewer::REVERSE_HOST`): only the app's program has both, so the command-line viewer does not offer the button (`Hello.switch_sides` false).
- **Controlled computer (agent)**: `client/agent/src/reverse.rs` relays the stream to a UDP socket on the loopback (`bridge_to_udp`, which only takes the first local program that writes to it) and starts this program as `viewer 127.0.0.1:PORT --fingerprint F`, with the password and the technician's name in `NUVIRA_PASSWORD` and `NUVIRA_REVERSE_OF`: the certificate is pinned, the password checked. On Windows, as SYSTEM (the service's agent), the window is started with the token of the person signed in (`WTSQueryUserToken`, `CreateProcessAsUserW` on `winsta0\default`, their environment block plus the two variables); otherwise as the agent itself (on Linux with `WAYLAND_DISPLAY`/`DISPLAY` from the session's sockets when a user service lacks them). It tells the viewer `SwitchSides { on }` when the window opened, `on: false` (with `error`) when it was closed or could not open. The stream ending (the technician took control back, or the session ended) closes the window.
- Offered (`Hello.switch_sides`) in sessions through a server, with a screen, by the app's program, not to guests; the web client neither offers nor answers it yet.
- Diagnosis: `NUVIRA_REVERSE_LOG=FILE` in the agent's environment writes the window's log there, `NUVIRA_REVERSE_ARGS` adds options to it (tests: `--input-test`); `NUVIRA_REVERSE_AS_USER` takes the Windows path of the service without being SYSTEM. Viewer test options: `--switch-after SECONDS`, `--switch-back-after SECONDS`.

## Tests (2026-11-15)

Through the NAS's server; the picture checked with screenshots, the input with the technician's pointer moved by the window's `--input-test` (between ¼ and ¾ of the screen's width, read at the technician's):

| Technician | Controlled computer | Result |
|---|---|---|
| PC-FEDERICO, Windows 11, NVIDIA encoder | PC-LABORATORIO, Windows 10, agent as the user | window "Schermo di PC-FEDERICO — NuviraHub", 8–10 fps (a still screen), 13–17 ms; pointer moved; taken back after 40 s, the window closed; closed at the controlled computer, the technician told |
| PC-FEDERICO | PC-LABORATORIO, the **installed service** (SYSTEM) with this build | the window started as the user signed in (`PC-LABORATORIO\mrbla`), picture; taken back, the window gone; the service put back as it was (0.27.0 signed, its `service.json`) |
| PC-FEDERICO | Linux Mint X11 | window, picture (screenshot), pointer moved |
| PC-FEDERICO | Ubuntu 26.04 GNOME Wayland | window, pointer moved |
| PC-FEDERICO | Kubuntu 26.04 KDE Wayland | window, pointer moved |
| Linux Mint X11 | PC-LABORATORIO | Mint's screen (1280x800) at PC-LABORATORIO, Mint's pointer moved |
| Ubuntu GNOME Wayland | PC-LABORATORIO | the portal's permission restored from its token (no dialog), picture |
| Kubuntu KDE Wayland | PC-LABORATORIO | the same, 26 fps; the viewer's own device key was locked in KWallet, so a key file was used (`--identity`) |

The technician's window minimized during the switch and back afterwards (screenshots of PC-FEDERICO).

More tests the same day:
- **The pointer on Wayland as the technician**: the positions the compositor reports for the technician's pointer (the capture's metadata, logged at trace by the window) went to ¼ and ¾ of the width as the window moved it: GNOME 320/959,400, KDE 319/959,399.
- **Through the relay** (`--transport websocket`): 15 runs from PC-FEDERICO in a row, 10 more as the first stalled ones (switching after 8 s, 40 s long), the Linux technician on Mint 3 runs with a moving picture (screenshots 2 s apart show the clock's second hand moving in the window at PC-LABORATORIO). On the way: a run that looked stuck was a still screen (X11 sends nothing new; the clock tried first showed only its unchanging date), found with the inner connection's numbers (RTT 5–9 ms, window 333 kB, no loss, no flow-control block, logged at debug every 5 s) and a stack dump of the technician's process under gdb (capture waiting for changes).
- **In a row**: 15 runs on the LAN, all streaming.
- **With a guest**: PC-FEDERICO switched while Mint was a guest; the guest kept its picture (10 to 59 fps) all along. After a handover the new first viewer switched sides (see 39-shared-sessions.md).
- Seen twice in about 60 runs, not since: the window at the controlled computer stayed black, the technician's video channel open but no frame (once on the LAN, once through the relay, both before the diagnosis logs were added). The pipeline now logs its first captured image, its first encoded frame and an encoder that takes nothing for 3 s (debug), and the inner connection's numbers, to tell where it stops if it comes back.

Tried on the Mac mini M1 (Scaleway, macOS 27.2.1, 2026-11-16, the app built from the branch, personal unattended installation):
- the Mac as the controlled computer: PC-LABORATORIO's app (`nuvira.exe viewer --switch-after 8 --switch-back-after 20`) switched; the Mac's LaunchAgent agent opened the window "PC-LABORATORIO's screen — NuviraHub" showing LAB's desktop, and closed it when the technician took control back;
- the Mac as the technician: the Mac app's viewer (started with `open -n -a NuviraHub --args viewer …`, so that it has the app's Screen Recording permission) switched with PC-LABORATORIO's service; LAB opened the window with the Mac's screen (its title shows the Mac's host name, a UUID on this rented Mac), then "ended by the technician" on schedule.
