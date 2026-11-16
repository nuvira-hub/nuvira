# NuviraHub — Invitation links (NuviraHub Pro)

[← Index](README.md)

A technician sends a customer a link. The link opens NuviraHub, or downloads
it, and asks that technician for support: the request goes to the support
queue ([21-support-queue.md](21-support-queue.md)) reserved for them. As for
every support request, the person at the computer still accepts the
connection.

Pro feature `pro.invite`, module `invite`. It comes with the accounts'
entitlement (`ADDRESS_BOOK`), the same as the queue.

## The technician

- The app's **Queue** section, "Invitation links":
  - choose the validity (1 hour, 4 hours, 1 day, 1 week) and the uses (1, 5
    or 20), add a note, then "Create the link";
  - the link is copied, and shown with its code (e.g. `abcde-fghjk`);
  - the list shows the invitations (an administrator sees them all) with their
    validity, the uses and the note, a copy button and Delete.
- API calls: `CreateInvite { hours, uses, note, server }`, `ListInvites`,
  `DeleteInvite { code }`. Limits: 1 to 720 hours (default 24), 1 to 100 uses
  (default 1). Codes are 10 characters without 0/o/1/l/i (about 50 bits).
- The link is `NUVIRA_PUBLIC_URL/i/CODE` when the server knows its public
  address. Otherwise the app builds it from the server's address:
  `http://HOST:PORT/i/CODE`, on the same port as the WebSocket and the updates.
- A request for this technician shows "Invited by you" in the queue. Another
  technician sees "For NAME" and has no Take button; the server refuses them
  anyway (`@server.support_reserved`).
- While the technician works in another section, the app checks the queue every
  15 s. When a new request for them arrives, the Queue icon gets a dot and the
  window asks for attention (taskbar or dock), without taking the focus.

## The customer

- **The page** `/i/CODE`:
  - written in the browser's language (5 languages);
  - shows who invites, the note, and how to get help: "Open NuviraHub"
    (`nuvirahub://invite/CODE?server=HOST[:PORT]&key=FINGERPRINT`), the code
    to type by hand, and the downloads;
  - the visitor's system is put first;
  - an invitation that is not valid shows a page saying so (404), the same for
    unknown, expired and used-up codes;
  - security headers: CSP `default-src 'none'`, no-referrer, no-store.
- **Windows download**: the program in the server's updates folder, named
  `NuviraHub-host=…,port=…,key=…,invite=CODE.exe`. It starts with the server
  and the invitation, and asks for support without installing anything (the
  "server in the file name" of the open edition). With no program there, the
  latest GitHub release.
- **macOS, Linux**: GitHub's dmg and the site's download page. Then "Open
  NuviraHub" from the page again, or the code typed by hand.
- **The app** registers `nuvirahub://` when it starts:
  - Windows: HKCU\Software\Classes;
  - Linux: a hidden `.desktop` file and xdg-mime; the packages' desktop file
    has the scheme too;
  - macOS: `CFBundleURLTypes` in the Info.plist.

  Plugins: `tauri-plugin-deep-link`, and `tauri-plugin-single-instance` so that
  a link, or the downloaded program, started while the app runs reaches the
  running app; the second start then closes.
- **On the same server** the request goes at once ("Request sent to NAME: wait
  for them to connect"). **On another server** the card asks first: "Use this
  server and ask for support". This is the same as saving the server in the
  settings (the unattended access moves with it, the ID may change). Where the
  server is locked (customised program, organisation's rule) the card says so.
- **By hand**: Ask for support → "Invitation code" field (capitals, spaces and
  dashes are ignored).
- An invitation that is not valid any more: "The invitation is not valid any
  more (expired or already used): ask the technician for a new one".
- The same computer asking again with the same code within 12 hours (cancel,
  reconnecting) spends no other use.
- **Restart and update**: the new start of the app waits for the old one to end
  (`NUVIRA_WAIT_PID`). Otherwise, with one app at a time, it would hand over to
  the old one and close.

## Tests of 2026-11-15

Pro test server on the Ubuntu VM. Technician on Mint (app, admin) and on
PC-LABORATORIO. Customer on PC-LABORATORIO (its service, as SYSTEM, replaced by
this build for the tests) and on Mint (user service).

| Test | Result |
|---|---|
| Mint app: "Create the link" (1 day, once, note) | link `http://192.168.178.41:21216/i/jb3wq-t5gpk` shown and copied; listed with its validity and "used 0 of 1" |
| The page from the NUC (curl, Windows browser, Italian) | heading, note, `nuvirahub://` link with server and fingerprint, code, Windows download first |
| `/i/CODE/windows` | 38.9 MB, the program of the updates folder (same bytes), `NuviraHub-host=192.168.178.41,port=21216,key=…,invite=jb3wqt5gpk.exe` |
| PC-LABORATORIO: `start nuvirahub://invite/…` with the app open | HKCU registration to the installed program; the link reached the running app; request for admin (use 1 of 1) |
| Mint queue | "Invited by you", Take; LAB accepted; session (8 fps, 10 ms); the invitation card gone, "ended · used 1 of 1" |
| Edge on PC-LABORATORIO: the page, then "Open NuviraHub" | dark theme, Italian; Edge asked "This site is trying to open NuviraHub" → Open → the app came to the front and asked |
| PC-LABORATORIO: the downloaded `NuviraHub-…invite=….exe` started while the app runs | the code reached the running app (second start's file name), request (use 1 of 5) |
| PC-LABORATORIO: code typed `G4NAC JPKS6` | request for admin; same computer: no other use spent |
| Mint technician in another section | dot on the Queue icon; X11 urgency hint set |
| PC-LABORATORIO: used-up invitation | the card: "The invitation is not valid any more…" |
| Mint as customer, public server, link for the Pro test server (xdg-open) | "The invitation is for the server …; NuviraHub now uses …" → Use this server: the user service rewritten for the Pro server, request sent once online (use 2 of 5); LAB took it, Mint accepted, session |
| Firefox on Mint: the page (Linux first, light theme) → Open NuviraHub | Firefox asked to open the nuvirahub link with NuviraHub → one app, request sent |
| Restart: a start with `NUVIRA_WAIT_PID` | Linux waited 3.1 s, Windows 11.9 s for the old app, then the only one |
| Delete from the list (LAB) | the page answers 404 |
| Account "anna" (technician) on PC-LABORATORIO | the request shows "For admin", no Take button; REST `GET queue` with her token: `"for_account": "admin"` |
| A server's restart | the queue is empty (it lives in memory) and the customer's app still shows "sent": a limit the queue already had; with an invitation the request has to be made again |

Found and fixed by these tests:
- the invitation card's text sat next to its title;
- the customer saw the request's state twice (invitation card and support card);
- the list of invitations did not refresh its uses;
- the code wrapped on the page;
- "now uses NuviraHub" instead of "the public NuviraHub server";
- the downloaded program started while the app runs lost its code.
- the invitation card had no way to withdraw the request (now "Cancel the request"; tried on PC-LABORATORIO: the server withdrew it).

Unit tests: `invite` (codes, use and expiry, defaults and limits, lists and
delete by owner/admin, licence, public URL, the page in it/de/fr, invalid page,
download redirect, file name), `supportqueue.TestInvitation`, `invite::tests`
and `file_config` (app).

On the Mac mini M1 (Scaleway, macOS 27.2.1, 2026-11-16; app built there from the branch, signed with a fixed test identity) (Pro test server running on the Mac): Safari opened the page
(English, macOS download first), "Open NuviraHub" → Safari asked "Do you want to
allow this website to open NuviraHub?" (the scheme registered by `CFBundleURLTypes`)
→ the running app got the link and asked for support (use 1 of 1); PC-LABORATORIO
took it, the Mac asked "the technician who took your support request" → Accept →
session (VideoToolbox). Found and fixed: the invitation's card stayed out of sight
when the app was on another section (now the connection section comes up).

On the Mac mini M1 (Scaleway, macOS 27.2.1, 2026-11-16), the app closed (the unattended agent running, as on every Mac
with unattended access): Safari → "Open NuviraHub" → Allow → **"The application
NuviraHub is not open anymore"**, no app. The agent of unattended access is the
same program in the same bundle, so Launch Services counted it as the running
NuviraHub (`lsappinfo`: pid of `nuvira agent`, type Foreground) and sent it the
link, which it did not handle. The same was true of NuviraHub opened from the
Dock, Finder or `open -a`: it went to the agent and nothing opened. Fixed
(`client/app/src/mac_links.rs`): a person's agent keeps its main thread in an
`NSApplication` loop as an accessory app (no Dock icon), takes the link (GURL
Apple event) and the "reopen" (its delegate's `applicationShouldHandleReopen`;
NSApplication answers 'rapp' itself, so an Apple event handler for it is never
called) and starts the app with `open -n` (with the link as an argument); the app
takes a link from its arguments on macOS, and a second start hands it to the
running app. Tried after the fix:

| Test | Result |
|---|---|
| App closed: Safari → Open NuviraHub → Allow | the agent: "link opened while the app is closed: starting the app"; the app started with the invitation's card; request (use 1 of 2); PC-LABORATORIO took it ("Invited by you"), the Mac asked "the technician who took your support request" → Accept → session |
| App open: the same link | to the app directly, request sent, no other use spent (same computer) |
| App closed: `open -a NuviraHub` | the agent: "NuviraHub opened while only the agent runs: starting the app" → the app |
| The agent's other work (sessions, capture, clipboard files, wallpaper) | unchanged (the tests of this day ran with it) |

Not tried yet:
- macOS: the same with the managed installation (its agent is the same program,
  so the same path), and a click on the Dock's icon by a person (`open -a` and
  the Dock both send "reopen");
- found on 2026-11-16: while an update (here the test rebuild) replaced the bundle,
  the agent started by launchd crashed in `NSApplication` (CFBundleCopyExecutableURL
  on a missing bundle: "nuvira quit unexpectedly"). Fixed: without a whole bundle
  the agent runs without taking links; launchd starts it again afterwards;
- the Linux polkit/package paths on Fedora's .rpm for the scheme (the .rpm's
  desktop file is the same file as the .deb's).

More tests of 2026-11-16:

- **Behind a reverse proxy.** A test HTTPS proxy (Go `httputil.ReverseProxy`:
  TLS, `X-Forwarded-For/-Proto`, WebSocket) in front of the Pro test server on
  the Mac, `NUVIRA_PUBLIC_URL=https://51.159.121.104:21443`: the app's "Create the
  link" gave `https://51.159.121.104:21443/i/yk578-2sw7r`; the page through the
  proxy in Chrome (Italian), its "Open NuviraHub" link carrying the agents' server
  (`51.159.121.104:21216`), not the proxy's; the Windows download through the
  proxy: 200, 38.9 MB, the file name with server, key and code.
- **Chrome** (157, PC-LABORATORIO): "Apri NuviraHub" → Chrome's own question
  "Aprire NuviraHub? … vuole aprire questa applicazione" → "Apri NuviraHub" (UI
  Automation) → the running app got it, request "Invited by you", use 1 of 1.
  The page's script cannot `fetch` (CSP `default-src 'none'`), as intended.
- **Server locked** (PC-LABORATORIO, `HKLM\SOFTWARE\Policies\nuvira` `Server`
  set, app restarted), a link for the NAS's server: "L'invito è per il server
  192.168.178.12:21116, ma su questo computer il server non si può cambiare."
  Found and fixed: with an earlier request still waiting, the card offered
  "Cancel the request" (that earlier one) under the new invitation's text; now
  "Close", which drops the invitation and leaves the earlier request (tried:
  position 1 kept). Rule removed afterwards.
- **The server's refusal from the app**: an invitation of anna's used by the Mac;
  PC-LABORATORIO's app (admin) shows it "For anna" without Take; `support_take`
  called in the app through WebView2's DevTools (what the button would do):
  `@server.support_reserved` (5 languages: "This request is for the technician who
  sent the invitation"), the request not taken.
- **Linux packages.** The branch's .deb, .rpm and .tar.gz built with
  `tools/package-linux.sh` in the jammy container with nfpm 2.47.0 (as the CI):
  - Kubuntu (KDE Wayland), the .deb installed: before any start of the app
    `xdg-mime` already answers `io.github.nuvira-hub.nuvira.desktop`
    (`mimeinfo.cache`: `x-scheme-handler/nuvirahub`); a link with the app closed
    (`xdg-open`) started it with the link ("invitation received
    other_server=true"); a second one with the app open went to it (one app);
  - found and fixed: the app then wrote its own hidden handler (the deep-link
    plugin's `nuvira-handler.desktop`, `Exec="/usr/bin/nuvira" %u`) and made it the
    default. `xdg-open` without a desktop environment (generic mode, as on a window
    manager alone) does not understand the quotes and gave the link to Firefox
    (same file without quotes: the app got it). Now the app writes the handler
    itself, unquoted when the path needs no quotes, and nothing when the package's
    desktop file is there for `/usr/bin/nuvira` (an earlier handler is removed and
    the package's made the default again: "the package's handler again"). Tried:
    Kubuntu with the new .deb, generic mode and Plasma's environment → the app;
    the tar.gz on Mint: `Exec=/home/alessandro/nuvira-tar/nuvira-0.27.0/nuvira %u`,
    generic mode and Cinnamon's environment → the app.

Not done yet: the invitations in the web console (list, delete); a request kept
in the queue across a server's restart.
