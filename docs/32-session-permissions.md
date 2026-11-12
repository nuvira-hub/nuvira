# NuviraHub — Session permissions

[← Index](README.md)

What a session may do is decided in three places, each of which can only take something away:

1. **The settings of the controlled computer** (app → Settings → This computer): privacy mode, terminal, tunnels, printing, recordings. They hold for every session.
2. **The server's rules and the permissions of the technician** (NuviraHub Pro): the rules for every device, the limits of the technician's account, of its membership in the shared address books that hold the computer, or of the sessions started without an account. The server sends them to the agent with the connection request.
3. **The person at the computer, during the session**: from the strip of the app they switch off (and back on) keyboard and mouse, clipboard, files, terminal, tunnels, printing, the change of the screen resolution and the computer's sound, for that session only.

Whatever one of them takes away is not available, and nothing the earlier ones do not allow can be switched on later.

## At the controlled computer: the strip

While a session is in progress the strip at the top of the app has a second row, "Allowed in this session:", with a button for each thing the session has: Keyboard and mouse, Clipboard, Files, Terminal, Tunnels, Printing, Screen resolution, Sound (only those the session offers: a terminal-only session shows Terminal and Tunnels). A click switches it off (the button is crossed out), another click allows it again. The choice is for that session only: the next one starts with everything the rules allow.

Switching off acts at once:

- **Keyboard and mouse**: the agent stops injecting the viewer's input, and the keys still pressed are released (the input thread ends, see `input_thread` in `client/agent/src/lib.rs`); switching it on again opens a new one.
- **Clipboard**: nothing is copied any more in either direction (text and images).
- **Files**: requests of the file manager are refused, and the transfers in progress stop (the receiving or sending task ends; the viewer reads "the person at the remote PC switched file transfers off").
- **Terminal**: open terminals end, with everything the shell started; new ones are refused.
- **Tunnels**: open connections through the tunnels are cut; new ones are refused.
- **Printing**: documents printed during the session are no longer sent to the technician.
- **Screen resolution**: a screen the session had adapted to the technician's window goes back to its own resolution, and new requests are refused ([37-remote-resolution.md](37-remote-resolution.md)).
- **Sound**: the agent stops capturing the computer's sound and sends none; the viewer's sound button is greyed out ([38-remote-audio.md](38-remote-audio.md)).

The app sends `AgentCommand::Switch { name, on }` to its agent (or to the service's one, through the local channel); the agent answers with `AgentEvent::Permissions { offered, off }`, also repeated to an app opened during the session.

## At the technician's: the viewer

The agent tells the viewer what the session may do with `Permissions` (`proto/nuvira/v1/session.proto`), after the video format and at every change: `input`, `clipboard`, `files`, `terminal`, `tunnel`, `print`, `resolution`, `audio`, and `switched_off` with what the person at the computer switched off. The viewer then:

- without keyboard and mouse, sends no input (the "view only" button is pressed and locked, as is Ctrl+Alt+Del: "Keyboard and mouse are not allowed in this session");
- hides the buttons of the file manager, of sending files, of the terminal and of the tunnels when they are not allowed;
- stops sending its clipboard;
- shows "Switched off by the person at the remote computer: …" (it stays on screen) and "Allowed again by the person at the remote computer: …".

The agent does not rely on the viewer: what is not allowed is refused at the agent, also to older viewers or to a modified one.

## NuviraHub Pro: permissions of the technicians

With a Pro server whose licence has the address book (`ADDRESS_BOOK`, feature `pro.limits`) the administrator decides what the sessions of each technician may not do. The same list of limits is used everywhere: always confirm at the computer, no keyboard and mouse (view only), no files, no clipboard, no privacy mode, no terminal, no tunnels, no printing, no restart, no change of the screen resolution, no sound of the computer, no recordings by the technician, record every session (with the recordings on the server).

- **Per account** (console → Accounts, "What the sessions of this account may not do"; app → Administration → Permissions next to the account): every session of that account.
- **Per member of a shared address book** (console → Shared books, under the member; app → Administration, editing the book, Permissions next to the member): the sessions of that account with the computers of that book. A computer in several shared books gets the limits of all of them; the personal address book does not count, since the app adds to it every computer reached. Only administrators change these limits: whoever manages a book saves its members without touching them.
- **Sessions without an account** (console → Rules, "Permissions of the sessions"; app → Administration, "Sessions without an account…"): viewers not signed in to the server, for example someone given the permanent password of a computer, or a sign-in that has expired. An administrator who restricts the technicians should restrict these sessions at least as much, otherwise not signing in avoids the limits.

A session gets the server's rules, the account's limits and those of the books together: the most restrictive wins. "Record every session" wins over "no recordings by the technician". Changes hold from the next session; the rules for all the devices are still sent at once to the connected ones.

**How the limits reach the computer.** The viewer presents the sign-in of the app that started it (`Connect.account_token`, as for the session log). The server works out the limits of that account towards that computer (`ext.SessionLimiter`, `SessionLimits` in the Pro module) and sends them to the agent with the request (`Incoming.limits`, a `Policy`); the agent adds them to the server's rules for that session only (`merge_limits`). The limits never travel through the viewer, which could leave them out.

**Older agents.** Agents from before this version do not know `Incoming.limits`. They announce signalling protocol 1 (`Register.protocol_version`; from this version 2): when a session has limits the server refuses it with "the remote computer has an older NuviraHub that cannot apply the permissions of your account: update it" (`@server.agent_too_old`), rather than letting it start with every permission. Sessions without limits work as before. The new rules for all the devices (no keyboard and mouse, no restart, no recordings) are ignored by older agents.

## Limits

- Switching off in the strip needs the app at the controlled computer: with the service and nobody signed in, nobody can switch anything off (the server's limits still hold).
- The permissions protect the controlled computer from what NuviraHub does; they are not a sandbox. With keyboard and mouse a technician can open a command prompt or copy files with the tools of the computer itself.
- Privacy mode, restart and recordings are not switches of the strip: the black screen covers it, the restart ends the session, the recordings have their own setting in the app.
- The web client of NuviraHub Pro applies the agent's refusals but does not show the `Permissions` notices yet.

## Tests

2026-11-07, Pro test server on the Ubuntu VM with this build; test accounts made through the console API (`tec1` with no terminal and no files on the account, `tec2` with no clipboard as a member of the shared book holding the computer).

Linux Mint 22.3 (X11) as the controlled computer, a test agent with `--ipc` and a stand-in for the app on its socket (it sends `switch` commands); viewer on Fedora 44 (Wayland):

- `tec1`: the agent logged the limits of the session, the viewer received `files: false, terminal: false`, the app `offered` without files and terminal.
- `tec2`: only the clipboard off (the book's limit), not the account's limits of `tec1`.
- Switched off during the session: keyboard and mouse, then the terminal, then keyboard and mouse back on together with files off; every change reached the viewer at once ("Switched off by the person at the remote computer: …").
- An open terminal (`--terminal`, `sleep 60` running) ended when the terminal was switched off: the viewer read the reason in the terminal, the shell was gone.
- With `--input-test` the pointer of Mint moved between 320 and 959 every second, stopped when keyboard and mouse were switched off and moved again when they were switched on.
- A 1.5 GB file sent with `--send-file` stopped at about 300 MB when files were switched off, with the reason in the viewer.
- Sessions without an account with "no keyboard and mouse": the viewer got `input: false`, the pointer did not move.
- The agent of an earlier build (signalling protocol 1): `tec1` refused with the text of `@server.agent_too_old`, a session without limits to the same agent went through.

PC-LABORATORIO (Windows 10) with this build installed as the service (NAS server, so no limits from the server) and its app open; viewer on Fedora with `--input-test`:

- The strip showed "Allowed in this session: Keyboard and mouse, Clipboard, Files, Terminal, Printing" (window capture of the app). Pressed through UI Automation (the buttons are toggles for it: `aria-pressed`), keyboard and mouse were crossed out, the agent stopped injecting (the cursor stayed at 959 except for the few pixels of the USB mouse jiggler on that PC) and the viewer showed the notice; pressed again, the cursor alternated 320/959 again.
- Files switched off during a 1.5 GB transfer: stopped, and the partial file in Downloads was removed.

Go tests: masks, account and member limits, several books, the personal book not counting, sign-out and unknown tokens, anonymous limits only by administrators, a manager saving the book does not change the limits, limits kept when absent. Rust test of the switches.

Not tested yet: the console pages and the app's Administration against a real server (only rendered with demo data and through the console API), printing switched off during a print job, tunnels switched off with a connection open, macOS.
