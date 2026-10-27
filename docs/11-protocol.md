# NexoDesk — Connection protocol

[← Index](README.md)

> **Status.** This page is the original draft of the flows. The implemented messages are in [`proto/nexodesk/v1/rendezvous.proto`](../proto/nexodesk/v1/rendezvous.proto) (device ↔ server, address book included) and [`proto/nexodesk/v1/session.proto`](../proto/nexodesk/v1/session.proto) (viewer ↔ agent); the implementation is described in [13-phase1-network.md](13-phase1-network.md). Main differences: TLS 1.3 between the devices' keys instead of Noise (D10), the session password checked by the agent (D13), no capability negotiation yet.

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
| `control` | reliable stream | hello, authentication, video format, screens, quality, feedback for the adaptive bitrate, clipboard, chat, file outcome, close |
| `input` | reliable stream | mouse, keyboard, wheel, Ctrl+Alt+Del |
| `video` | reliable stream (planned: datagrams) | encoded frames, keyframe requests on the control channel |
| `file` | one stream per file | name and size, then data |
| `audio` | datagrams (planned) | Opus |
| `tunnel` | one stream per connection (planned) | TCP port forwarding |
| `terminal` | reliable stream (planned) | PTY |

With the WebSocket fallback the same QUIC connection runs inside the WebSocket tunnel, so the channels are unchanged.

## 5. Capabilities and permissions (planned)

Capabilities that can be requested: `view`, `control`, `clipboard`, `file`, `audio`, `terminal`, `tunnel`, `system` (restart, services), `record`.

- The effective capabilities are the intersection of: the technician's request ∩ the server's policy ∩ the remote user's consent.
- The remote device applies the final decision: even a compromised server cannot grant more than the user/the local policy allows.
- Every capability change produces an audit event signed by both sides.

## 6. Versioning

- `Hello` and `Register` carry the protocol version; the highest common one is used.
- Protobuf fields are only added, never reused; unknown messages are ignored.
