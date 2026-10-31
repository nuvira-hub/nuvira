# NuviraHub

Remote desktop and remote support that you can host yourself. Website: **[nuvirahub.dev](https://nuvirahub.dev)**.

- **App** for Windows (Rust; Tauri for the main window, a native wgpu window for the session): attended support with confirmation, unattended access through a system service that also handles administrator prompts (UAC), the lock screen and Ctrl+Alt+Del. Multiple screens, clipboard, chat, file transfer, a local address book, automatic signed updates. macOS and Linux are planned.
- **Server** (Go): a single Docker container for signalling, relay and app updates, on UDP and TCP port 21116 (TCP for the WebSocket fallback on networks that block UDP).
- **End-to-end encryption**: sessions are QUIC/TLS between the two devices, authenticated with their Ed25519 keys; the server never sees screen, input, clipboard or files, even when it relays the traffic.
- **Five languages**: English, Italian, French, German, Spanish.

## Download

- **App**: from the [releases](https://github.com/nuvira-hub/nuvira/releases/latest): `nuvira-setup.exe` (installer) or `nuvira.exe` (runs without installing).
- **Server**: `ghcr.io/nuvira-hub/nuvira-server` (linux/amd64, linux/arm64), see [deploy/](deploy/README.md).

With no server set, the app uses the free NuviraHub public server `connect.nuvirahub.dev` ([terms](https://nuvirahub.dev/en/terms/), [privacy](https://nuvirahub.dev/en/privacy/)). To keep everything on your side, run your own server and enter its address in the app's settings.

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

Open port 21116 (UDP and TCP) towards the server and back up the `nuvira-data` volume: it holds the server's identity, pinned by the apps. Details in [deploy/README.md](deploy/README.md).

## NuviraHub Pro

Server address books shared by a team, accounts and roles, central rules, web console, two-factor authentication, single sign-on, session log and custom installers are NuviraHub Pro modules: a separate server image that needs a licence. See [nuvirahub.dev](https://nuvirahub.dev/en/pricing/). The app is the same for both servers.

## Building

- Server: `cd server && go test ./... && go build ./cmd/nuvira-server` (Go 1.27+), or the [Dockerfile](server/Dockerfile).
- App: `cd client && cargo build --release -p nuvira-app` (Rust stable, Windows); `tools/build-setup.sh` builds the installer.
- Protocol: `proto/` (Protocol Buffers), generated code with `tools/gen-proto.sh`.

The [documentation](docs/README.md) describes architecture, protocol, security and every part of the app.

## Contributing and security

Bug reports and ideas are welcome in the [issues](https://github.com/nuvira-hub/nuvira/issues); before a pull request read [CONTRIBUTING.md](CONTRIBUTING.md). Security problems: please do not open a public issue, see [SECURITY.md](SECURITY.md).

## Licence

Copyright (C) 2026 Alessandro Blason.

NuviraHub is free software, released under the **[GNU Affero General Public License v3.0 or later](LICENSE)** (AGPL-3.0-or-later). Anyone who modifies it and makes it available to others, even only over a network, must publish the source of their version under the same licence.

As an additional term under section 7(b) of the licence, the author credits shown in the interfaces ("NuviraHub © 2026 Alessandro Blason · AGPL-3.0 · Source code") must remain visible and unaltered in every copy and modified version: see **[ADDITIONAL-TERMS.md](ADDITIONAL-TERMS.md)**.
