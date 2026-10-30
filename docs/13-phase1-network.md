# NuviraHub — Phase 1: server and connection over the Internet

[← Index](README.md)

Goal ([08-roadmap.md](08-roadmap.md)): two devices on different networks connect through `nuvira-server`, directly (P2P) when possible, otherwise through the relay, with real authentication instead of a hand-copied fingerprint.

## Choices

| # | Choice | Reason |
|---|---|---|
| D10 | **End-to-end encryption with QUIC's TLS 1.3 between the two devices**, with certificates bound to the devices' Ed25519 keys (mutual verification), instead of a second Noise handshake | One handshake and one encryption layer; same security (mutual authentication, forward secrecy). The server vouches for the ID ↔ public key binding |
| D11 | **Relay at the UDP datagram level** (like TURN): the server forwards the already encrypted QUIC packets between the two devices | The same QUIC code works directly and through the relay; the relay cannot read anything |
| D12 | **A single UDP port on the server** for signalling (QUIC) and relay: packets from addresses registered for the relay are forwarded, the others go to the server's QUIC | One port to open in the firewall/NAT (plus TCP for the WebSocket fallback) |
| D13 | Session password checked by the agent **inside** the encrypted channel; the server never sees it | Intermediate step towards the PAKE planned in [06-security.md](06-security.md) |

## Components

```
            ┌──────────────────── nuvira-server (Go) ──────────────────────┐
            │  UDP :21116                                                    │
            │   ├─ demux: address bound to a relay session? ─► relay         │
            │   └─ otherwise ─► QUIC (quic-go): signalling                   │
            │  SQLite: devices (ID, public key, last seen)                   │
            └────────────────────────────────────────────────────────────────┘
               ▲ presence + requests             ▲ relay datagrams
   agent ──────┘                                  └────── viewer
     │◄═══════════ end-to-end QUIC (TLS with the devices' keys) ═════════►│
```

- **Device identity**: Ed25519 key pair created at first start and saved in the user's data folder. The device's TLS certificate is self-signed with this key.
- **ID**: 9 digits assigned by the server at the first registration, bound to the public key.

## Flows

### Registration and presence (agent)

```
agent                                server
  │── Register{pubkey, name, os} ──────►│
  │◄────────────── Challenge{nonce} ────│
  │── Proof{Ed25519 signature(nonce)} ─►│  checks the signature; existing or new ID
  │◄──── Registered{device_id} ─────────│
  │      (connection kept open: QUIC keep-alive, incoming requests)
```

### Connection (viewer → agent)

```
viewer                         server                          agent
  │── Connect{ID, viewer pubkey} ─►│                               │
  │                                │── Incoming{session, viewer ──►│
  │                                │   pubkey, viewer name}        │
  │                                │◄── Accept{session} ───────────│
  │◄── Route{session, relay,       │                               │
  │    agent pubkey}               │                               │
  │  (each side opens a UDP socket and sends RelayBind{session, role} to the relay)
  │════ QUIC viewer→relay→agent, certificates checked against the pubkeys ═══│
  │── Auth{password} (inside the encrypted channel) ────────────────►     │
```

The relay pairs the two addresses that sent `RelayBind` for the same session and from then on forwards every datagram from one to the other.

### Direct (P2P)

In the relay bind each side adds its own local address (the interface it uses to reach the server, with the port of the session socket). When both are bound, the relay tells each side the other's candidates:

- the address **seen by the relay**, through any NAT (hole punching between different networks);
- the **local** addresses (same network behind the same router, where the public address is often unreachable: hairpin).

Both sides send opening packets to all candidates; the viewer tries QUIC towards each of them in parallel with the relay, and the first direct connection wins. The relay is used only if no direct connection succeeds within 1.5 s from when the relay is ready.

Unlike `rustdesk-server`, which recognises devices behind the same router by their identical public IP and only then asks for the local address, here local candidates are always tried: there is no need to guess the topology, and it also covers cases where the public IP seen by the server does not match (CGNAT with several exits, IPv4/IPv6).

Test: `NUVIRA_RELAY_FAKE_REFLEXIVE=1` on the server replaces the address seen by the relay with an unreachable one (203.0.113.1); the connection must still succeed directly through the local address.

## Steps

| # | Step | Status |
|---|---|---|
| 1 | Go server: QUIC signalling, registration with signature, SQLite, 9-digit IDs. Agent: `--server`, prints ID and password | ✅ |
| 2 | Connection request, UDP relay, viewer connecting by ID with verified keys, session password | ✅ |
| 3 | Automated tests through the server in a LAN (`NUVIRA_VIA_SERVER=1`, `NUVIRA_FORCE_RELAY=1`) | ✅ |
| 4 | Docker image (`ghcr.io/alexbn71/nuvira-server`, GitHub Actions) | ✅ image; publishing on the Internet postponed |
| 5 | Hole punching and path choice (direct preferred, relay as fallback) | ✅ |
| 5b | Local addresses as candidates (devices behind the same router, hairpin) | ✅ |
| 6 | WebSocket fallback for networks that block UDP | ✅ |

## Usage

```powershell
nuvira-server.exe                                   # UDP 21116, data in .\data
nuvira-agent.exe --server 192.168.1.10              # prints ID and password
nuvira-viewer.exe --server 192.168.1.10 123456789 --password abcd2345
```

At the first connection agent and viewer remember the fingerprint of the server's certificate (`servers.txt` in the data folder) and then refuse a server with a different certificate; `--server-key` pins it by hand.

## LAN measurements (2026-10-26)

NUC (agent + nuvira-server) → PC-LABORATORIO (viewer), real screen with animation, Quick Sync.

| Path | fps | Latency p50 | p95 | RTT |
|---|---|---|---|---|
| Direct | 32 | 37 ms | 46 ms | 1.3 ms |
| Relay (forced) | 33 | 43 ms | 52 ms | 1.7 ms |

## WebSocket fallback (networks that block UDP)

Instead of a second session protocol over TCP (like RustDesk's `hbbr`, which forwards TCP/WebSocket connections), NuviraHub carries **the same QUIC datagrams inside a WebSocket**:

- **client** (`client/transport/src/ws.rs`): a socket for quinn that sends and receives datagrams through the WebSocket. QUIC, end-to-end encryption, key verification and relay stay identical. Direct path attempts are skipped (UDP does not get through on these networks).
- **server** (`server/internal/relay/websocket.go`): each WebSocket connection becomes a virtual address in the same dispatching as the UDP port; the relay pairs a UDP side with a WebSocket side with no difference.
- **Choice**: `--transport auto` (default) tries UDP for 4 s and then switches to `ws://<server>:21116/nuvira`; `--transport udp|websocket` forces one; `--server-ws wss://…` gives the URL published behind a reverse proxy. The session uses the same transport as signalling.
- Server: `NUVIRA_WS_LISTEN` (default `:21116` on TCP, `off` to disable it), path `/nuvira`.

Local tests (agent and viewer on the same PC, 1280x720 at 30 fps):

| Agent → viewer | First frame | Latency p50 | p95 |
|---|---|---|---|
| UDP → UDP | 0.8 s | 13 ms | 41 ms |
| UDP → WebSocket | 1.4 s | 14 ms | 42 ms |
| WebSocket → WebSocket | 0.6 s | 18 ms | 44 ms |
| UDP blocked, automatic choice | 5.5 s | 25 ms | 44 ms |

Between the two PCs (NUC → PC-LABORATORIO through nuvira-server, Quick Sync, animated screen):

| Viewer | Path | fps | Latency p50 | p95 |
|---|---|---|---|---|
| UDP | direct | 29 | 38 ms | 49 ms |
| WebSocket | relay through the tunnel | 28 | 39 ms | 57 ms |
| Automatic with UDP blocked | relay through the tunnel | 29 | 38 ms | 50 ms |

In a LAN the WebSocket tunnel costs very little; over the Internet the TCP under QUIC gets worse with losses (double retransmissions), which is why it stays a fallback.

Fixes that came up: QUIC's close message goes through a queue drained by a task, and the viewer exited before it was sent (the agent stayed busy until the timeout); orderly shutdown of the viewer (wgpu, window, network and decoding threads) after sporadic crashes on exit. 30 consecutive sessions alternating UDP and WebSocket without errors.

## Degraded network: adaptive bitrate and BBR (2026-10-26)

Simulation on the relay (`NUVIRA_RELAY_IMPAIR`, tests only), all traffic forced through the relay, real screen with animation, 25 s.

| Simulated network | Before (fixed bitrate, Cubic) | After (adaptive + BBR), hardware | After, software |
|---|---|---|---|
| Clean (LAN) | 37 ms | 38 ms (p95 49) | 59 ms (p95 78) |
| 25 ms, 1% loss, 5 Mbit/s | 2.2 s (p95 4.7 s) | **78 ms** (p95 130) | 90 ms (p95 147) |
| 40 ms, 2% loss, 2 Mbit/s | 3.7 s (p95 9.6 s) | **100 ms** (p95 202) | 127 ms (p95 222) |

What was done:
- **Feedback from the viewer** every 250 ms: median and minimum latency of the displayed frames.
- **Adaptive bitrate** in the agent (`client/agent/src/abr.rs`): it drops by 20% if the median latency of the last second exceeds the minimum of the last 10 s by 60 ms for at least a second, rises by 15% if it stays close; 1.5 s of settling after every change. `--fixed-bitrate` disables it.
- The latency used is the **network part only** (end of encoding → arrival at the viewer, `Feedback.transit_ms_*`, `VideoFrame.encoded_us`): in the first Internet test (2026-10-27, mobile network through the relay) a slow encoding on the controlled PC (130 ms per frame) was taken for congestion and the bitrate fell to the minimum.
- **No feedback**: the bitrate goes down only if a frame left more than 2 s ago with no feedback since. Before, it was measured from the last frame, and a still screen (one frame every few seconds) looked like a stuck network.
- **Still screen**: the bitrate changes only while the video sends at least a quarter of the bitrate or 150 kbit/s. On a mobile network sparse traffic alone makes latency jump (30–185 ms with 1 kbit/s sent); before this rule the bitrate fell from 4 to 0.5 Mbit/s in 16 s with nothing moving.
- **Single peaks**: with the screen moving, the mobile network still had 150–600 ms peaks every few seconds while 0.3 Mbit/s were sent on a 143 Mbit/s link; reacting to each one brought the bitrate to the minimum (blurry photos). Only a queue lasting a second counts now. On congestion the bitrate goes to 85% of the rate actually sent when that is lower than 80% of the cap: the encoder often stays well below the cap, and lowering only the cap changed nothing. Simulated on the relay (synthetic source, Quick Sync, 30 s): with `stall=300ms,every=3s` the bitrate climbs to the maximum in 7 s with no decrease (p50 75 ms); with `rate=1mbit` it drops at once below 1 Mbit/s, p50 102 ms and p95 1158 ms (before: 118 and 1975 ms).
- **Quick Sync ignores bitrate changes once the session has started** (Media Foundation): the new bitrate is applied by restarting the encoder (~0.3 s), with hysteresis (drops ≥ 15% at most once per second, rises ≥ 25% every 3 s). OpenH264 changes on the fly.
- **BBR** as QUIC's congestion control: with random losses NewReno/Cubic used less than half of the bandwidth (`NUVIRA_CC_CUBIC=1` to compare).
- **QUIC send buffer limited to 256 KB**: beyond that, the agent skips frames instead of queueing them.
- Simulator: delay, jitter without reordering, loss, limited bandwidth with a finite queue.

## What changes over the Internet

The OPNsense configuration (forwarding the server's UDP port) only makes the server reachable; the software still has to be checked under conditions a LAN does not present:

- **Symmetric NATs and CGNAT** (mobile networks): the direct path may fail and the session stays on the relay; how often remains to be measured.
- **Hairpin**: two devices behind the same NAT reaching the server through the public IP. Solved with local candidates (above); verified in a LAN by simulating an unreachable public address.
- **Networks that block UDP** (companies, hotels): WebSocket fallback (above). To get through corporate proxies and firewalls it must be published as `wss://` on port 443 behind the reverse proxy.
- **Packet loss, jitter, RTT of 30–100 ms**: handled with adaptive bitrate and BBR (section above), verified in simulation; the real test remains.
- Smaller **MTUs** (VPN, PPPoE): QUIC handles them, to be verified.

Some of these points can be simulated in a LAN (artificial loss and delay) before the real test.
