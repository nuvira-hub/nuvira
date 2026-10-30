# NuviraHub — Architecture

[← Index](README.md)

> **Implementation status.** This is the original design. Where the implementation took another route, the decision log ([10-decisions.md](10-decisions.md)) and the phase documents describe what was built: sessions use **TLS 1.3 end-to-end with the devices' Ed25519 keys** instead of Noise (D10), the relay works on UDP datagrams (D11), the server listens on **UDP 21116 plus TCP 21116** for the WebSocket fallback instead of 443 (D12), the session toolbar is a small WebView instead of egui (D5), and the address book is managed from the app (D16; no web console yet).

```
                    ┌──────────────────── "nuvira-server" container ─────────────────────┐
                    │                                                                     │
 Technician ───────►│  TLS gateway :443 (HTTP/2, WebSocket, QUIC on UDP :443/udp)         │
                    │   ├─ Rendezvous / Signalling  (presence, NAT candidate exchange)    │
                    │   ├─ Built-in STUN            (public address discovery)            │
                    │   ├─ Relay                    (QUIC/TCP/WSS, blind: encrypted only) │
                    │   ├─ REST + gRPC API          (devices, address book, policies)     │
                    │   └─ Administration web console (not a remote client)               │
                    │                                                                     │
                    │  Storage: SQLite (default) or PostgreSQL · Recordings: volume/S3    │
 Assisted client ──►│                                                                     │
                    └─────────────────────────────────────────────────────────────────────┘
           ▲
           └──────────── Direct P2P (QUIC with hole punching) when possible ───────────┘
```

### 3.1 Server (Docker)

- **Language**: Go (decision D4; static binary, image < 30 MB). The protocol code is generated from `proto/` for both Go and Rust.
- **Modules**, all in the same binary, each one can be enabled or disabled:
  - `signal` — device presence (persistent WebSocket/QUIC connection), ICE-like candidate exchange, push notifications to the device to reach.
  - `stun` — reflexive address answer on the UDP port.
  - `relay` — pairs two encrypted streams by session token; bandwidth limits per tenant/user; several relay instances possible in different regions.
  - `api` — REST/JSON + OpenAPI; user authentication (password + TOTP + passkey/FIDO2 + OIDC for SSO).
  - `console` — administration SPA served by the binary (address book, devices, groups, policies, reports, recordings, customised installation packages).
  - `audit` — hash-chained log (cannot be altered without it showing).
- **Configuration**: environment variables + YAML file; server key generated automatically at first start.
- **TLS**: built-in Let's Encrypt or behind a reverse proxy, with QUIC passed through directly.
- Optional **multi-tenancy** (for a technician managing several customer companies).

Reference `docker-compose.yml` of the design (the current one is [deploy/docker-compose.yml](../deploy/docker-compose.yml)):

```yaml
services:
  nuvira:
    image: nuvira/server:latest
    ports:
      - "443:443/tcp"
      - "443:443/udp"
    volumes:
      - ./data:/data
    environment:
      NUVIRA_PUBLIC_HOST: remote.example.com
      NUVIRA_DB: sqlite:///data/nuvira.db
    restart: unless-stopped
```

### 3.2 Desktop client

- **Rust core** (performance, memory safety, excellent libraries for QUIC, codecs, cryptography, cross-compilation).
- **Capture, encoding, decoding and input are all in the Rust core**, with each system's native APIs (table below). The interface framework takes no part in it: capture quality does not depend on Tauri.
- **Hybrid interface** (decision D5):
  - **Management windows with Tauri v2**: address book, settings, ID/password, connection confirmation, chat, file transfer, about. One UI codebase for the three systems, small binary.
  - **Native remote session window, without a webview for the video**: a `winit` window + **wgpu** rendering (Direct3D 12 / Metal / Vulkan) showing the decoded frames without memory copies and without going through a webview.
  - Reason: the webview is the weak point for real-time video. WebView2 and Safari could cope (WebCodecs), but on Linux WebKitGTK has no reliable hardware decoding; every frame passed from the core to the webview adds copies and latency. With the native window latency is the same on all three systems.
  - Alternatives considered: everything in Flutter (RustDesk's choice, video rendered through a native texture, but a second language, Dart); everything in Slint/egui (no webview, management interface harder to build and less polished).
  - Tauri v2 supports Windows, macOS and Linux with the same code; it uses the system webview, which sets these minimum requirements:

    | System | Webview used by Tauri | Minimum NuviraHub version | Limit set by |
    |---|---|---|---|
    | Windows | WebView2 (Edge, already present on Windows 10/11; can be installed by the installer) | Windows 10 1903 | Windows.Graphics.Capture |
    | macOS | WKWebView | macOS 12.3 | ScreenCaptureKit (Tauri alone would reach 10.15) |
    | Linux | WebKitGTK 4.1 | Ubuntu 22.04, Debian 12, Fedora 36 or equivalent | WebKitGTK 4.1 required by Tauri v2 |

  - Differences between webviews (Edge/Safari/WebKitGTK) only affect the interface: it must be tested on the three systems at every release.
  - **Credits** (licence, [ADDITIONAL-TERMS.md](../ADDITIONAL-TERMS.md)): "NuviraHub © 2026 Alessandro Blason · AGPL-3.0 · Source code" visible in the main window and in the "About" section.
- **Processes**:
  - `nuvira` — user application (UI, outgoing sessions, "on-the-fly support" mode); the session window is the separate `nuvira-viewer` process.
  - `nuvira-service` — system service for unattended access:
    - Windows: LocalSystem service that starts the agent in the active session and on the **secure desktop** (UAC, sign-in screen, Ctrl+Alt+Del).
    - macOS: LaunchDaemon + LaunchAgent (sign-in screen included).
    - Linux: systemd unit; graphical session through the portal or the display manager.

| Function | Windows | macOS | Linux |
|---|---|---|---|
| Screen capture | Windows.Graphics.Capture / DXGI Desktop Duplication | ScreenCaptureKit | PipeWire (ScreenCast portal) on Wayland, XShm/XDamage on X11 |
| Input | SendInput (+ service for the secure desktop) | CGEvent (Accessibility permission) | RemoteDesktop portal / libei on Wayland, XTest on X11, uinput as a fallback |
| Hardware encoder | NVENC, Intel QSV, AMD AMF, Media Foundation | VideoToolbox | VAAPI, NVENC |
| Audio | WASAPI loopback | ScreenCaptureKit audio | PipeWire |
| Virtual monitor | IddCx driver | CGVirtualDisplay | compositor virtual output / xrandr |

### 3.3 Transport and protocol

- **QUIC** (`quinn` in the client, `quic-go` in the server) as the only transport:
  - separate streams for control, input, clipboard, files → a large file does not block the mouse;
  - unreliable datagrams for video → no retransmission delays;
  - connection migration (Wi-Fi → 4G without dropping).
- **Attempt order**: direct LAN → P2P with UDP hole punching → QUIC relay over UDP → relay over TCP/WebSocket (company networks that block UDP).
- **Messages**: Protobuf, with a negotiated protocol version.
- **End-to-end encryption**: **Noise** handshake (IK/XX) between the two devices, Ed25519 identity keys, **hybrid X25519 + ML-KEM key exchange** (quantum resistant). The server only signs the ID ↔ public key binding; clients check the fingerprint (TOFU + warning if it changes). *(Implemented instead as TLS 1.3 between the devices' keys, decision D10.)*

### 3.4 Video

- Negotiated codec: **hardware H.264 / H.265 / AV1** when available, AV1 or VP9 in software as a fallback.
- Encoding of changed areas only, adaptive bitrate and frame rate based on RTT and loss.
- **Sharp text mode**: for offices/terminals, 4:4:4 chroma or lossless encoding of static tiles.
- Hardware decoding on the client, wgpu rendering, cursor drawn locally (zero perceived latency).

### 3.5 Ready for a future web client

The web client is out of scope for now (D2), but the architecture is designed to add it without rewrites (D9). The web client will only be able to **control** other devices: a browser can neither be controlled remotely nor act as an unattended agent.

**What is reused**

| Part | Desktop | Browser | Reuse |
|---|---|---|---|
| Management windows | Tauri v2 (HTML/JS) | the same interface served as a web page | almost total: only the calls to the core change (Tauri commands → WASM module) |
| Session window | native wgpu | wgpu compiled to WebAssembly, drawing on a WebGPU canvas (WebGL2 as a fallback); the HTML toolbar as is | same Rust code |
| Protocol, cryptography, session state | Rust crates | the same crates compiled to WebAssembly | same Rust code |
| Video decoding | native hardware | **WebCodecs** (the browser's hardware decoding) | only the decoding module changes |
| Transport | QUIC (P2P or relay) | **WebTransport** (HTTP/3) or WebSocket towards the relay | only the transport module changes |

**Rules to follow from the first line of code**

1. The `proto`, `crypto` and `session` crates depend neither on the operating system nor on threads/sockets directly: they must also compile for `wasm32-unknown-unknown`. The specific parts (capture, codec, input, network) are behind interfaces (traits) with one implementation per platform.
2. The transport is a single abstraction (reliable streams + datagrams) with three implementations: QUIC, WebTransport, WebSocket. The session protocol does not know which one it is using.
3. The server relay accepts **WebTransport and WebSocket** from the start (in Go: `quic-go` + `webtransport-go`). Minimal cost today, no server change tomorrow.
4. The controlled device always offers at least one codec every browser can decode: **H.264** (besides H.265/AV1/VP9 when both sides support them).
5. Identical end-to-end encryption, in WebAssembly. The server stays blind for the web client too. The browser's private key goes into non-extractable IndexedDB storage (WebCrypto) or a passkey.

**Known browser limits** (accepted, to be documented for users)

- No direct P2P connection: the browser always goes through the relay (unless WebRTC data channels, which allow hole punching, are added in the future).
- Key combinations caught by the browser (Ctrl+W, Ctrl+T, Windows key): manageable only in full screen with the Keyboard Lock API (Chrome/Edge); on Firefox and Safari toolbar buttons are needed.
- Clipboard: text and images only, with the browser's permission; no copy/paste of files (file transfer is used instead).
- Requirements: a recent browser with WebCodecs and WebTransport (Chrome/Edge; Firefox and Safari have WebCodecs, WebTransport to be checked at development time; otherwise WebSocket is used).
