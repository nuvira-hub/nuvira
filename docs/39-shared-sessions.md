# Shared sessions

[← Index](README.md)

While a technician is connected, others can join the same session: a colleague called in to help, a second-level technician watching, someone checking from the browser. Each one reaches the computer the usual way (its ID, a password, the confirmation by the person at the computer when it is required), and then sees the same screen and can use the same keyboard and mouse.

## Who has what

| | The first viewer (who started the session) | Viewers who joined (up to 3) |
|---|---|---|
| Picture | yes | yes, the same frames |
| Keyboard and mouse | yes | yes, through the same input (the person's "Keyboard and mouse" switch and the technician's limits apply to each) |
| Choice of screen, resolution, quality | yes | no: they follow the first viewer |
| Files, clipboard, terminal, tunnels, printing, sound, recording, privacy mode, restart | yes | no |

The agent tells a joining viewer, in its `Hello`, that only the picture and the keyboard are there: any viewer can join (the app's viewer, the web client of NuviraHub Pro, older versions), it shows only what it is offered. When the first viewer's session ends, the others end with it.

**The first technician is told.** Whoever started the session sees "PC-B joined this session: they see the screen and can use the keyboard and mouse" when someone joins, and "PC-B left the session" when they leave (the app's viewer and the web client, from 0.26.0; the control message `Guests`, ignored by older viewers).

## At the computer

- **Asked like any other.** A request arriving during a session goes through the same access control: with the temporary password and confirmation on, the person at the computer is asked ("Incoming connection") and can refuse; with the permanent password the rules decide as for the first one.
- **The strip says who.** "PC-STUDIO-NUC and NuviraHub Web are controlling this computer · direct connection": the app's strip names every viewer, and comes to the front when someone joins. "End" ends the whole session, everyone's.
- With the rule "Record every session", nobody can join: a joining viewer does not record, and the recording is the first viewer's.
- A terminal-only request during a session is refused as before ("the device is busy in another session"), and so is a fourth guest.

## How

- **Agent** (`client/agent/src/shared.rs`): the session in progress puts a `Shared` in the agent's context once its picture is running (sessions through a server; LAN tests stay single). A request from the server while busy is accepted when a session can be joined; its connection goes through the same `Gate` (password, confirmation), then `guest_session` runs: the current `VideoFormat`, `Permissions` (input only), a video stream, the guest's input stream into the session's input loop (one injector for everyone, so the switch and the release of pressed keys work for all), `Ping`/`Pong` and `RequestKeyframe`.
- **Video.** The encoder runs once. Each frame sent to the first viewer is copied to every guest's queue (8 frames). A guest that falls behind loses frames, skips everything until the next keyframe, and asks for one; keyframes for guests are asked every 2 s at most, so a slow guest cannot fill the first viewer's network with them. The bitrate and the size of the picture follow the first viewer (its network, its window). A change of screen or resolution sends the new `VideoFormat` to the guests too.
- **Video on a stream** for guests, never as datagrams.
- **First viewer.** The same list as the control message `Guests { names }` (proto, field 32 of `ControlMessage`), at every change; the viewer compares it with the last one, counting each name (two windows of one computer have the same name), and shows who joined or left.
- **App.** `AgentEvent::Guests { names }` at every change (empty when the last one leaves), repeated to an app that opens during the session; the strip uses `session.strip_shared`.
- **Server.** No change: each viewer's request is a session of its own for the server (its own route, relay and report); a guest's report has no transfers.

## Tests

2026-11-13, PC-LABORATORIO (Windows 10):

- A test agent of this branch (through the NAS's server), the NUC's desktop viewer connected first, then the web client (headless Edge 157 on the bench) with the same ID and password: the web client joined ("viewer joined the session guests=1"), showed the picture at 9 to 10 fps at the first viewer's size (900x720), with no files, terminal or sound buttons. The web client moved the mouse: PC-LABORATORIO's cursor went to the two points (read by a task in the user's session). When the desktop viewer's 35 s were over, the web client's session ended too ("viewer left the session").
- First try: the guest's mouse did nothing while the first viewer had not used its own input yet (the agent opened the input loop only with the first viewer's input stream, which shows with its first event). Fixed: the guests' events open the injector by themselves.
- The installed app and service of this branch on PC-LABORATORIO (the 0.23.1 installation put back afterwards): with the desktop viewer and the web client connected, the app's strip read "PC-STUDIO-NUC e NuviraHub Web stanno controllando questo computer · connessione diretta" (screenshot of the user's session), and the guest's mouse worked through the service's agent.

- 2026-11-13, a Mac mini M1 (macOS 27.0, release 0.25.0, installed for the whole Mac): the NUC's viewer first, the web client joined; the guest saw the Mac's desktop and moved its pointer; the app's strip on the Mac read "PC-STUDIO-NUC and NuviraHub Web are controlling this computer · direct connection".

- 2026-11-13, branch `guests-notice`: PC-LABORATORIO's test agent, the NUC's viewer first; the web client, then two more desktop viewers joined (three guests, "guests=3" in the agent's log); a fourth was refused ("the device is busy in another session"). The first viewer's log: joins and leaves of each, also of the two with the same name.
- 2026-11-13, a Linux agent (Linux Mint VM, X11) of the branch with `--ipc`, its temporary password, and a stand-in for the app on its socket answering the requests: the first viewer accepted; a second viewer refused at the computer ("access denied: connection not accepted on the device"); a third accepted, joined and saw the screen; the stand-in got `guests` with the name, then empty; the first viewer was told of the join and the leave.
- 2026-11-13, the web client as the first viewer (bench, headless Edge) and a desktop viewer joining: the page showed "PC-STUDIO-NUC si è unito alla sessione…", then "…ha lasciato la sessione".
- A slow guest, in a unit test (`shared::tests::slow_guest_waits_for_a_keyframe`): its queue of 8 frames full, the next is dropped, it waits for a keyframe and one is asked for, at most once in 2 s; the frames it had queued still come, nothing is shown until the keyframe.

Not tested yet: a guest on a real slow network.
