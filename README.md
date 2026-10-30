# NuviraHub

Self-hosted remote control.

- **Server** (Go): a single Docker container for signalling, relay and the address book, on UDP and TCP port 21116 (TCP for the WebSocket fallback on networks that block UDP).
- **Desktop client** (Rust; Tauri v2 for the app, a native wgpu window for the session): Windows today, Linux and macOS planned. Attended support with confirmation, and unattended access through a system service that also handles administrator prompts (UAC), the lock screen and Ctrl+Alt+Del.
- **End-to-end encryption**: sessions are QUIC/TLS between the two devices, authenticated with their Ed25519 keys; the server never sees screen, input or files, even when it relays the traffic.
- **Interface** in English, Italian, French, German and Spanish.

Status: Windows client and server working in a LAN (video with hardware or software H.264 and adaptive bitrate, mouse and keyboard, multiple screens, clipboard, chat, sending files, unattended service, central address book with accounts). Not yet tested across the Internet; no installer yet. See the [documentation](docs/README.md) and the [roadmap](docs/08-roadmap.md).

## Licence

Copyright (C) 2026 Alessandro Blason.

NuviraHub is free software, released under the **[GNU Affero General Public License v3.0 or later](LICENSE)** (AGPL-3.0-or-later). Anyone who modifies it and makes it available to others, even only over a network, must publish the source of their version under the same licence.

As an additional term under section 7(b) of the licence, the author credits shown in the interfaces ("NuviraHub © 2026 Alessandro Blason · AGPL-3.0 · Source code") must remain visible and unaltered in every copy and modified version: see **[ADDITIONAL-TERMS.md](ADDITIONAL-TERMS.md)**.
