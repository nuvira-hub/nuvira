# NuviraHub — Goals and constraints

[← Index](README.md)

1. **A single server container** (plus an optional database) on **one UDP port, plus the same port over TCP** for the WebSocket fallback: easy to publish behind NAT or a reverse proxy.
2. **End-to-end security**: the server cannot see screen, input or files.
3. **One client** for Windows, Linux (X11 and Wayland) and macOS, with an "on-the-fly support" mode (no installation) and an "unattended" mode (service).
4. **Low latency**: target < 50 ms glass-to-glass in a LAN, < 120 ms over the Internet with P2P.
5. **Central management** in the free server: users, address book, groups, policies, audit.
6. Code **written from scratch** (RustDesk only as a study reference; see decision D3).
7. **Open source, AGPL-3.0-or-later**, with mandatory author credits in the interfaces (decision D6).
8. **Interface in five languages**: English by default, Italian, French, German, Spanish (decision D15).
