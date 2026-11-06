# NuviraHub — Web client

[← Index](README.md)

A NuviraHub Pro server with the web client in its licence (feature `pro.web_client`) serves a page at `/web/` on its TCP port (the one of the WebSocket fallback, 21116 by default). From that page a technician controls computers with only a browser: no app to install, for example from a colleague's PC or a customer's laptop. The browser controls other computers; it cannot be controlled itself (as planned in [03-architecture.md](03-architecture.md#35-ready-for-a-future-web-client)).

## What it does

- **Connecting.** By ID and password, like the app, with "Terminal only" too. Signing in with an account of the server (password, two-factor code, single sign-on) shows the address books: a click on an entry connects with the password saved there. The passwords of the entries stay inside the page's WebAssembly: the page's script only learns that an entry has one.
- **Session.** The remote screen in the page, mouse (buttons, wheel) and keyboard by physical key, as the viewer sends them (`KeyboardEvent.code`); Ctrl+Alt+Del for Windows; choice of the screen (also all of them at once) and of the quality; full screen. The agent scales the picture to the size of the page.
- **Clipboard (text).** Ctrl+V in the session, or the "Paste" button, sends the text of the local clipboard first; text copied on the remote computer lands in the local clipboard while the page has focus. Images are not shared.
- **Chat** with the person at the remote computer.
- **Files.** A file manager of the remote computer: folders, download (the file is saved by the browser), upload into the open folder. Files dropped on the page go to the remote Downloads folder.
- **Terminal.** The remote command prompt in the page (xterm.js, as in the viewer), from the toolbar or as a terminal-only session.

The agent sees an ordinary viewer whose system is `web`; the central rules and the agent's own settings apply in the same way (files, clipboard, terminal, confirmation).

## How it works

The page runs the viewer's network side compiled to WebAssembly: the same QUIC (quinn) and TLS 1.3 with rustls as the desktop. They travel inside a WebSocket to `/nuvira`, exactly like the desktop's fallback for networks that block UDP ([13-phase1-network.md](13-phase1-network.md)):

- the page checks the server with the fingerprint of its certificate, which the page itself gets from the server (`/web/config.json`);
- the browser has its own Ed25519 device key, kept by the page (local storage of the browser), and registers as a device that receives no connections, like a viewer;
- the session is end to end between the browser's key and the agent's key: the server relays encrypted QUIC packets and sees nothing of them, as for any session through the relay;
- the browser decodes the H.264 video with WebCodecs, in hardware where the browser can.

Nothing changes in the server's protocol, in the relay or in the agents. The sign-in of the account is kept for the browser tab (session storage) until it is closed or the technician signs out.

## Requirements

- **NuviraHub Pro** with the entitlement `WEB_CLIENT` in the licence. Without it `/web/` only says that the licence does not include the web client.
- **HTTPS.** Browsers decode video only in a secure context, and a page received in clear could have been changed on the way. Like the console, the server goes behind a reverse proxy with HTTPS that forwards `/web/` and the WebSocket `/nuvira` (with the `Upgrade` header) to the TCP port. With Nginx Proxy Manager it is the same host as the console with "Websockets Support" on. Inside the LAN, `http://localhost` works for tests; other plain addresses show a warning and no video.
- **Browser.** A recent Chrome, Edge, Firefox or Safari (WebCodecs with H.264). The terminal works without WebCodecs.

## Limits

- **Always through the relay.** A browser has no UDP: no direct path between the two computers. On the same LAN the cost is small (round trip of a few milliseconds through the server); over the Internet it is that of the relay.
- **Keys the browser keeps.** Ctrl+W, Ctrl+T, Ctrl+N and the Windows key stay with the browser, except in full screen on Chrome and Edge (Keyboard Lock API). Ctrl+Alt+Del has its button.
- **Files.** A download is held by the browser until it is complete, then saved: very large files need the memory for them. No resuming an interrupted transfer (the app has it), no folders.
- **Not in the page yet:** privacy mode, remote restart and TCP tunnels (the last one cannot exist in a browser: it would need a local port).

## Tests (2026-11-06)

With a test bench that served the page on `localhost` and forwarded the WebSocket to the Pro server on the NAS, driven in a headless Edge:

- **PC-LABORATORIO** (Windows, service, sign-in screen): screen at 950×760 for a 1280×1024 monitor scaled to the page, 8–9 fps on changes, round trip 2–6 ms; keys reached the sign-in screen (space opened the PIN box and the digits went into it); Ctrl+Alt+Del button.
- **Mint 22.3 X11** (user service): screen, mouse (right click opens the desktop menu), keyboard and clipboard together (a new desktop folder named by typing and Ctrl+V: `webkbd'clip42`, the apostrophe being the Minus key on an Italian layout, as with the viewer), terminal (`echo web-terminal-$((6*7)); uname -n` → `web-terminal-42`, `VM-Mint-NuviraHub`), file manager (home folder listed, a file uploaded and downloaded back with the same content), chat received by the agent.
- **Fedora 44 Wayland** (user service, portal): screen.
- **Sign-in** on the Pro test server of a Linux VM: wrong password refused, address book shown, sign-in kept after reloading the page, sign-out.
- The Pro server itself serves `/web/` with the WebAssembly as `application/wasm` and the security headers; the page works under them (WebAssembly and WebSocket allowed, nothing else).

Then, the same day, the page served by the Pro server on the NAS behind its HTTPS proxy (Nginx Proxy Manager with Let's Encrypt, WebSocket on): `config.json` says HTTPS and licensed, `/nuvira` upgrades to a WebSocket; from Edge on that page Mint again (video, terminal, file manager up and down with the same content) and LAB (video).

Still to try: the page from outside the home network (phone on mobile data), Firefox and Safari, a Windows PC with a user signed in (files and terminal on Windows), a long session over the Internet.
