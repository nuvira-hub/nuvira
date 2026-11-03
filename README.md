<h1 align="center">NuviraHub</h1>

<p align="center">
  <b>Open-source remote desktop you can host yourself.</b><br>
  Help someone at their PC, or reach your own computers, through a server that is yours, or the free public one.
</p>

<p align="center">
  <a href="https://github.com/nuvira-hub/nuvira/releases/latest"><b>Download</b></a> ·
  <a href="https://nuvirahub.dev">Website</a> ·
  <a href="deploy/README.md">Your own server</a> ·
  <a href="docs/README.md">Documentation</a>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://nuvirahub.dev/img/app/en/connect-dark-900.webp">
    <img src="https://nuvirahub.dev/img/app/en/connect-light-900.webp" width="600" alt="NuviraHub: connecting to a computer by ID">
  </picture>
</p>

## Why NuviraHub

- **Your server, one container.** Signalling, relay and app updates in a single Docker image on one port (21116, UDP and TCP), for a NAS, a home server or a small VPS. Templates for Unraid included. Without a server of your own, the app uses the free public one.
- **End-to-end encrypted.** Sessions are QUIC/TLS between the two devices, authenticated with their Ed25519 keys. The server never sees the screen, the input, the clipboard or the files, not even when it relays the traffic.
- **Fast.** The two devices connect directly whenever the network allows it, and through the server's relay otherwise; on networks that block UDP, over WebSocket. On a local network: 6 ms median latency at 1920×1080, measured with a Linux viewer controlling a Windows PC.
- **Complete on Windows.**
  - Attended support, with the person at the PC confirming.
  - Unattended access through a system service, including administrator prompts (UAC), the lock screen and Ctrl+Alt+Del.
  - Code-signed program and signed automatic updates.
- **Everything a session needs.** Multiple screens, clipboard with text and images, chat, a file manager with resumable transfers, and a local address book.
- **Five languages**: English, Italian, French, German, Spanish.
- **Free software** under the AGPL.

## Platforms

| | Controls other computers | Can be controlled | Unattended access |
|---|:-:|:-:|:-:|
| **Windows** 10 and 11 | ✅ | ✅ | ✅ |
| **Linux**, Wayland session (Ubuntu, Fedora) | ✅ | ✅ | — |
| **Linux**, X11 session (Linux Mint, Xfce) | ✅ | ✅ | — |
| **macOS** | in testing | planned | planned |

Linux packages are `.deb`, `.rpm` and `.tar.gz`, for 64-bit Debian 12, Ubuntu 24.04, Mint 22, Fedora 40 or later. On Wayland the system asks the person at the computer to allow remote control the first time. Tested with GNOME and Cinnamon.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://nuvirahub.dev/img/app/en/incoming-dark-900.webp">
    <img src="https://nuvirahub.dev/img/app/en/incoming-light-900.webp" width="420" alt="The person at the PC confirms the connection">
  </picture>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://nuvirahub.dev/img/app/en/address-book-dark-900.webp">
    <img src="https://nuvirahub.dev/img/app/en/address-book-light-900.webp" width="420" alt="Address book">
  </picture>
</p>

## Download

From the **[latest release](https://github.com/nuvira-hub/nuvira/releases/latest)**:

- **Windows**: `nuvira-setup.exe` (installer) or `nuvira.exe` (runs without installing). Signed with a Certum code signing certificate.
- **Linux**:
  - `nuvira-linux-x64.deb`: `sudo apt install ./nuvira-linux-x64.deb`.
  - `nuvira-linux-x64.rpm`: `sudo dnf install ./nuvira-linux-x64.rpm`.
  - `nuvira-linux-x64.tar.gz`: extract it and run `./install.sh`, which installs for your user only and needs no sudo.
- **Server**: `ghcr.io/nuvira-hub/nuvira-server` (linux/amd64, linux/arm64).

With no server set, the app uses the free NuviraHub public server `connect.nuvirahub.dev` ([terms](https://nuvirahub.dev/en/terms/), [privacy](https://nuvirahub.dev/en/privacy/)).

## Your own server

```yaml
services:
  nuvira-server:
    image: ghcr.io/nuvira-hub/nuvira-server:latest
    restart: unless-stopped
    ports:
      - "21116:21116/udp"
      - "21116:21116/tcp"
    volumes:
      - nuvira-data:/data

volumes:
  nuvira-data:
```

1. Run `docker compose up -d`.
2. Open port 21116, UDP and TCP, towards the server.
3. Enter the server's address in the app's settings.

Back up the `nuvira-data` volume: it holds the server's identity, which the apps pin. Details, Unraid templates and updates for your own devices are in [deploy/README.md](deploy/README.md).

## NuviraHub Pro

Some modules are part of NuviraHub Pro: a separate server image that needs a licence, while the app stays the same. The modules are:

- address books shared by a team;
- accounts and roles;
- central rules;
- web console;
- two-factor authentication;
- single sign-on;
- session log;
- custom installers.

See [nuvirahub.dev](https://nuvirahub.dev/en/pricing/): during the Founders Programme Pro is free until the launch.

## Building

- **Server**: `cd server && go test ./... && go build ./cmd/nuvira-server` (Go 1.27+), or the [Dockerfile](server/Dockerfile).
- **App, Windows**: `cargo build --release -p nuvira-app` (Rust stable); `tools/build-setup.sh` builds the installer.
- **App, Linux**: the same, with the WebKitGTK 4.1, GTK 3, PipeWire and libxkbcommon development packages and clang; `tools/package-linux.sh` builds the packages.
- **Protocol**: `proto/` (Protocol Buffers); `tools/gen-proto.sh` generates the code.

The [documentation](docs/README.md) describes the architecture, the protocol, security and every part of the app.

## Contributing and security

Bug reports and ideas are welcome in the [issues](https://github.com/nuvira-hub/nuvira/issues). Before opening a pull request, read [CONTRIBUTING.md](CONTRIBUTING.md): contributors accept the [Contributor Licence Agreement](CLA.md) once, with a comment. For security problems, please do not open a public issue: see [SECURITY.md](SECURITY.md).

## Licence

Copyright (C) 2026 Alessandro Blason.

NuviraHub is free software, released under the **[GNU Affero General Public License v3.0 or later](LICENSE)** (AGPL-3.0-or-later). Anyone who modifies it and makes it available to others, even only over a network, must publish the source of their version under the same licence.

As an additional term under section 7(b) of the licence, the author credits shown in the interfaces ("NuviraHub © 2026 Alessandro Blason · AGPL-3.0 · Source code") must remain visible and unaltered in every copy and modified version: see **[ADDITIONAL-TERMS.md](ADDITIONAL-TERMS.md)**.
