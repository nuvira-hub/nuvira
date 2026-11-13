# NuviraHub — Project documentation

| # | Document | Content |
|---|---|---|
| 02 | [Goals and constraints](02-goals.md) | What NuviraHub must guarantee |
| 03 | [Architecture](03-architecture.md) | Docker server, desktop client, transport, video |
| 04 | [Features](04-features.md) | Complete list with priorities (MVP / 1.0 / later) |
| 06 | [Security](06-security.md) | Keys, end-to-end encryption, PAKE, audit |
| 07 | [Repository layout](07-repository-layout.md) | Organisation of the monorepo |
| 11 | [Protocol](11-protocol.md) | Registration and connection flows |
| 12 | [Phase 0: prototype](12-phase0-prototype.md) | How to try it, status and latency measurements |
| 13 | [Phase 1: server and network](13-phase1-network.md) | Server, registration, relay, connecting by ID |
| 14 | [Phase 3: system service](14-phase3-service.md) | Unattended access, UAC and lock screen |
| 15 | [Toolbar](15-toolbar.md) | Session commands, screen and quality changes |
| 16 | [Clipboard, chat and files](16-clipboard-chat-files.md) | Phase 2: shared clipboard (text, images), chat, file manager with resume |
| 17 | [Address book](17-address-book.md) | Accounts, personal and shared address books on the server |
| 18 | [Installation on Windows](18-installer.md) | One program, server in the file name, install from the app, NSIS setup |
| 19 | [Automatic updates](19-updates.md) | Signed versions from the own server, service and portable copies |
| 20 | [Linux](20-linux.md) | The app controls other computers; on Wayland it can be controlled too (portal, PipeWire); .deb, .rpm and archive |
| 21 | [Support queue and reports](21-support-queue.md) | NuviraHub Pro: the customer asks for help, the first free technician takes it; reports of the support time for billing |
| 22 | [Inventory, health and Wake-on-LAN](22-rmm.md) | NuviraHub Pro: what each computer is, its health, and waking it through another computer of the same network |
| 23 | [REST API of the Pro server](23-rest-api.md) | NuviraHub Pro: devices, address books, sessions, reports and the support queue for ticketing and management software |
| 24 | [Privacy mode](24-privacy-mode.md) | Black screen and blocked mouse and keyboard at the controlled PC while the technician works (Windows) |
| 25 | [Remote terminal](25-remote-terminal.md) | A command prompt of the remote computer, with or without its desktop; as the user signed in |
| 26 | [TCP tunnels](26-tcp-tunnel.md) | NuviraHub Pro: a local port that leads to an address of the remote computer's network (web panel, Remote Desktop) |
| 27 | [Remote restart](27-remote-restart.md) | Restarting the controlled computer from the session; the viewer waits and connects again by itself |
| 28 | [Web client](28-web-client.md) | NuviraHub Pro: controlling computers from a browser, with the same end-to-end encryption (QUIC and TLS in WebAssembly through the server's WebSocket) |
| 29 | [Remote printing](29-remote-printing.md) | Documents printed on the controlled computer's NuviraHub printer come out of the technician's printer |
| 30 | [MSI package and policies](30-msi-deployment.md) | Silent installation for Intune, Group Policy and RMM tools; ADMX template for server and permanent password |
| 31 | [Session recording](31-session-recording.md) | MP4 video of the session on the technician's computer, with a notice at the controlled one, which can refuse it |
| 32 | [Session permissions](32-session-permissions.md) | What a session may do: settings of the computer, limits per technician (Pro), switches of the person at the computer during the session |
| 33 | [macOS](33-macos.md) | Package, controlled Mac (ScreenCaptureKit, Quartz, permissions), keyboard, unattended access with a LaunchAgent |
| 34 | [Linux: installation for the whole computer](34-linux-deployment.md) | `nuvira service deploy` and /etc/nuvira/policy.json: the computer's own ID, for Ansible, Puppet and scripts |
| 35 | [macOS: installation for the whole Mac](35-macos-deployment.md) | `nuvira-macos.pkg` and a configuration profile (MDM): the Mac's own ID, settings from managed preferences |
| 36 | [Device key protected by the system](36-device-key.md) | DPAPI, Keychain, Secret Service, systemd-creds; migration of plain keys, hand-over to other accounts |
| 37 | [Remote resolution adapted to the window](37-remote-resolution.md) | The remote screen takes the window's size (Windows, macOS, Linux X11), put back at the end; switch at the PC, Pro rule |
| 38 | [Remote audio](38-remote-audio.md) | What the controlled computer plays comes out of the technician's speakers (Opus over QUIC datagrams); mute button; sent from Windows for now |
| 39 | [Shared sessions](39-shared-sessions.md) | More technicians on the computer of a session in progress: the same picture and input, the rest stays with the first one |
