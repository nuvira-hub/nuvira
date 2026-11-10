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

## Tests (2026-11-10): Safari and Firefox from outside the home network

From a Mac mini in a data centre (Scaleway, Paris; macOS 27.2) to the page served by the Pro server on the NAS behind its HTTPS proxy (`nuvira.alexbn.ddns.net/web/`, so through the Internet and the home router), towards Mint 22.3 X11 (a test agent registered on the NAS server with a permanent password):

| | Safari 27 | Firefox 159 |
|---|---|---|
| Page, connection by ID and password | yes | yes (a wrong ID length is caught by the page: "The ID has 9 digits") |
| Video through the relay | yes, round trip 27–30 ms | yes, round trip 27–34 ms |
| Mouse (moves and left click land at the right place on the remote screen) | yes | yes |
| Right click | reaches the remote computer as X button 3 (seen with `xinput test-xi2`) | the same |
| Keyboard | yes | yes |
| Terminal | yes (`echo safari-$((6*7)); uname -n` → `safari-42`, `VM-Mint-NuviraHub`) | yes (`firefox-42`) |
| File manager: download | yes, same MD5 (Safari asks once whether the site may download) | yes, same MD5 (500 KB) |
| File manager: upload | yes, same MD5 (300 KB) | yes, same MD5 (400 KB) |
| Chat | message sent and shown in the page | not tried |
| Clipboard, local → remote ("Paste" button) | **the session drops** (see below) | yes: Firefox shows its own "Paste" bubble, the session stays up, the text reached the remote clipboard |
| Clipboard, remote → local | did not arrive | did not arrive |

The right click reaches the remote computer as button 3; Cinnamon's desktop menu did not open in that first test (nor with a right click made on Mint with XTest), but it did later the same day with the touch long press below, so the first tests' clicks, not the page, were the matter.

### Problems found (fixed the same day)

Both are fixed in the page: Ctrl/Cmd+V takes the text from the browser's paste event (no clipboard request, no bubble), Safari's Paste button opens a small field to press Cmd+V in, and text copied on the remote computer that the browser does not let the page copy by itself comes with a "Copy" button (or, if even that is refused, a field to copy it by hand). Tried on Safari 27, Firefox 159 and Chrome 157 towards Mint: the text went both ways and the session stayed up.

What was seen before the fix:

- **Safari: reading the clipboard drops the session.** The "Paste" button and Cmd+V on the video call `navigator.clipboard.readText()`. Safari answers with its own "Paste" bubble, and while the bubble is open the page stops driving the connection: about 10 seconds later (the QUIC idle timeout of the page) the session ends with "connection lost: timed out". Seen three times out of three (button twice, Cmd+V once). Firefox shows a similar bubble but keeps the page running.
- **Remote clipboard does not reach the local one on Safari and Firefox.** The page writes it with `navigator.clipboard.writeText()` when the remote clipboard changes, outside a user gesture; Safari and Firefox refuse that (Chrome and Edge allow it while the page has focus), and the page ignores the error, so nothing tells the technician.

## Touch screens (2026-11-10)

A first try from the owner's Android phone on mobile data (Chrome, page of the NAS) connected and showed the screen at 32 fps, but the finger acted as a mouse with the button held: moving it drew a selection rectangle. Since then, as remote control apps on a phone:

- **tap**: left click where the finger is; **long press** (half a second): right click;
- **one finger moving**: the pointer follows it, no button;
- **double tap, keep the finger down and move**: drag with the left button (selecting, moving windows);
- **two fingers**: wheel, the content following the fingers;
- **Keyboard** button (shown only where the main pointer is a finger): opens the on-screen keyboard. A phone's keyboard gives the page characters, not keys, and the protocol carries physical keys, so each character becomes the key that makes it on the layout of the device's language (Italian, German, French, Spanish, US for the others), which the remote computer is expected to use too: "è" becomes the key right of P, and an Italian remote computer shows "è". Characters the layout cannot make (emoji, or a letter of another language) are left out;
- on narrow screens the toolbar is one row that scrolls sideways.

Mouse and pen keep the direct handling. A modifier (Shift, Ctrl, Alt, Cmd/Windows) whose release the browser lost is released on the remote computer at the next key or mouse event that says it is up: otherwise it stays down there and turns the keys into shortcuts. Tried with a headless Edge emulating a phone (touch events through the DevTools protocol) towards Mint, the events read on Mint with `xinput test-xi2`: tap = button 1, long press = button 3 (the desktop menu opened), one finger = motion only, double tap and drag = button 1 held over 10 motions, two fingers up = wheel down, keyboard `Ab1`, Backspace, Enter = Shift+A, B, 1, Backspace, Enter. With the page in Italian towards Mint with an Italian keyboard, `è é ò ç à ù ì @ # € [ ] { } + * ; : - _ / ? ' ! £ Ab1 Città` arrived as typed.

Still to try: the phone again with these changes, a phone on mobile data, a Windows PC with a user signed in (files and terminal on Windows), a long session over the Internet; the chat from Firefox.
