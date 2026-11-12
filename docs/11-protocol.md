# NuviraHub — Connection protocol

[← Index](README.md)

> **Status.** This page is the original draft of the flows. The implemented messages are in [`proto/nuvira/v1/rendezvous.proto`](../proto/nuvira/v1/rendezvous.proto) (device ↔ server, address book included) and [`proto/nuvira/v1/session.proto`](../proto/nuvira/v1/session.proto) (viewer ↔ agent); the implementation is described in [13-phase1-network.md](13-phase1-network.md). Main differences: TLS 1.3 between the devices' keys instead of Noise (D10), the session password checked by the agent (D13), no capability negotiation yet.

## 1. Identity

- **Device**: Ed25519 key pair generated at first start. `device_id` = 9-digit number assigned by the server, bound to the public key.
- **Technician user**: account on the server (password; TOTP/passkey planned). Sessions always start from a device; the account is for the address book, policies and audit.
- **Server**: its own key; it vouches for the `device_id ↔ public key` bindings.

## 2. Device registration

```
Client                                   Server
  │── Register{pubkey, os, version} ──────►│
  │◄──────── Challenge{nonce} ──────────────│
  │── Proof{signature(nonce)} ────────────►│
  │◄──── Registered{device_id} ─────────────│
```

The device then keeps a presence connection (QUIC, or WebSocket as a fallback) open, also used to receive connection requests.

## 3. Starting a session

```
Technician (A)           Server                      Remote (B)
  │── Connect{B} ─────────────►│
  │                          │── Incoming{A} ─────────────────────►│
  │                          │◄── Accept / Reject ─────────────────│
  │◄── Route{B's key, relay token} ──│── Route{A's key, relay token} ──►│
  │                                                                  │
  │═════════ parallel attempts: LAN → P2P UDP → relay ══════════════│
  │═════════ TLS 1.3 between the devices' keys ═════════════════════│
  │═════════ password (and confirmation by the person at B) ════════│
```

- **Candidates**: LAN addresses and the reflexive address seen by the relay.
- The first path that completes the handshake wins; the others are closed. The relay is opened right away in parallel so as not to lose time if P2P fails.
- The server only sees that A and B started a session, when and for how long: no content.

## 4. Channels inside the session (QUIC streams)

| Channel | Type | Content |
|---|---|---|
| `control` | reliable stream | hello, authentication, video format, screens, quality, feedback for the adaptive bitrate, clipboard (with the keys to paste it, `Clipboard.paste`, towards agents with `Hello.paste`, from 0.22.3), chat, file outcome, privacy mode, remote restart, close |
| `input` | reliable stream | mouse, keyboard (physical keys by W3C code), wheel, Ctrl+Alt+Del; text typed as characters (`Text`, only towards agents with `Hello.text`, from 0.22.2) |
| `video` | QUIC datagrams with forward error correction (`Hello.video_datagrams`), or a reliable stream with older devices | encoded frames, keyframe requests on the control channel |
| `file` | one stream per file | name and size, then data; from the agent also clipboard images, downloads and printed documents (see [29-remote-printing.md](29-remote-printing.md)) |
| `audio` | QUIC datagrams (`Hello.audio`, from 0.23.0) | Opus packets, `SetAudio` mutes (see [38-remote-audio.md](38-remote-audio.md)) |
| `tunnel` | one bidirectional stream per TCP connection (Pro) | raw bytes after `TunnelOpen`/`TunnelResult` (see [26-tcp-tunnel.md](26-tcp-tunnel.md)) |
| `terminal` | one bidirectional stream per terminal | shell in a pseudo-terminal (see [25-remote-terminal.md](25-remote-terminal.md)) |

With the WebSocket fallback the same QUIC connection runs inside the WebSocket tunnel, so the channels are unchanged.

## 5. Capabilities and permissions

The first plan, kept for reference; what was built is in [32-session-permissions.md](32-session-permissions.md) (the server's `Policy`, the limits of the session, the switches of the person at the PC, the agent's `Permissions` message). The signed audit events were not made.

Capabilities that can be requested: `view`, `control`, `clipboard`, `file`, `audio`, `terminal`, `tunnel`, `system` (restart, services), `record`.

- The effective capabilities are the intersection of: the technician's request ∩ the server's policy ∩ the remote user's consent.
- The remote device applies the final decision: even a compromised server cannot grant more than the user/the local policy allows.
- Every capability change produces an audit event signed by both sides.

## 6. Versioning

- `Hello` and `Register` carry the protocol version; the highest common one is used.
- Protobuf fields are only added, never reused; unknown messages are ignored (also by the server since 0.21.1: older ones answered an `Error`).

From 1.0 a change that breaks these rules is a major version (nuvira-pro/docs/internal/33-compatibility-1.0.md). The rules the protocol follows:

- **Features are announced before use.** Each side says in `Hello` what it can do (file manager, video datagrams, privacy mode, terminal, tunnel, restart, printing, recording), the server in `Registered.features`. A new feature gets a new flag and is used only when the other side set it.
- **The rules only take away.** `Policy` fields are `no_*` (or `confirm_always`, `record_always`): an older device that does not know a field allows what it allowed before. A new rule that must hold with older agents makes the server refuse the session to them, as `Incoming.limits` does with `Register.protocol_version` 2.
- **The handshake order is fixed.** On `control`: `Hello` from the viewer, `Hello` from the agent, then (when the agent asks for it) `Auth`, `ConfirmationPending` (viewers of protocol version 2 and later) and `AuthResult`. Older sides fail on anything else here: new steps go after `AuthResult`, or only between sides that both announced them.
- **Unidirectional streams are told apart by order.** The first one the viewer opens is `input`, the first one the agent opens is `video` (without datagrams); every later one starts with a `FileChunk`. A new kind of unidirectional stream is a `FileChunk` flag announced in `Hello` (as `clipboard_image` and `print` were): otherwise an older side saves it as a file. Bidirectional streams after `control` start with `StreamOpen`, which can have new kinds (an older agent closes a stream it does not know).
- **Error texts are keys.** `"@key"` messages are translated on the side that shows them: keep the keys, change only the translations.
- **One codec today.** The viewer does not say what it decodes: H.264 for all. A second codec needs a new `Hello` field with the list (absent = H.264 only).
- **ALPN.** `nuvira/0` between devices, `nuvira-signal/1` towards the server. They are labels and stay as they are.
