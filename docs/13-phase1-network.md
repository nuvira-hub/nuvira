# NuviraHub — Phase 1: server and connection over the Internet

[← Index](README.md)

Goal: two devices on different networks connect through `nuvira-server`, directly (P2P) when possible, otherwise through the relay, with real authentication instead of a hand-copied fingerprint.

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

### IPv6 (0.21)

- **One socket for both families.** The session's UDP socket is `[::]:0` with `IPV6_V6ONLY` off (dual-stack), so it reaches IPv4 addresses as IPv4-mapped ones (`::ffff:a.b.c.d`). Where the system has no IPv6 it is of the relay's family, as before. Addresses are compared in their plain form, so a mapped address and its IPv4 one are the same.
- **Local addresses of both families.** Each side gives one IPv4 and one IPv6 local address: the interface towards the relay, or towards a public address of that family. Link-local addresses (`fe80::/10`) and mapped ones are left out. On IPv6 the local address is also the public one: there is no NAT.
- **Announce on the second family.** When the server's name has both an A and an AAAA record, each side sends, before every bind, the same bind with the 0x80 bit on the role to the relay's address of the other family. The relay only notes the address it came from as one more candidate of that side, after its local ones, and does not answer. A side talking IPv6 to the relay so still gives its public IPv4 address (through its NAT), and the other way round. Servers before 0.21 ignore it.
- **Only families this PC has.** Direct attempts go only to the families this side has a local address of: on an IPv4-only network an IPv6 candidate is skipped (sending there fails with "network unreachable").
- **Logs.** The agent's log says "directly over IPv6" or "directly over IPv4"; the viewer's has `family` on its `path` line.
- **Relay across families.** The relay forwards between an IPv4 side and an IPv6 one (one socket, dual-stack in Go).

Tests of 2026-11-09, test server of this version on the Mac mini at Scaleway (public IPv4 and IPv6), agent with a synthetic picture on the Mac, viewer on PC-LABORATORIO (home network, IPv4 only, behind NAT):

| Case | Path | fps | latency p50 |
|---|---|---|---|
| Mac on the relay over IPv4, announcing IPv6 | direct IPv4 | 30.2 | 33 ms |
| Mac on the relay over IPv6, viewer over IPv4 | direct IPv4 (the Mac's local IPv4 address) | 29.8 | 37 ms |
| The same with `NUVIRA_FORCE_RELAY=1` | relay, IPv4 ↔ IPv6 | 29.9 | 33 ms |
| LAN, PC-LABORATORIO agent ↔ NUC viewer, NAS server 0.20 | direct IPv4 | 29.5 | 21 ms |
| The same with `NUVIRA_FORCE_RELAY=1` | relay | 29.8 | 23 ms |
| Linux viewer (Mint, package of the CI) → Mac agent | direct IPv4, 7 times out of 8 (once the relay: the direct attempt did not answer within the 1.5 s; the old viewer the same right after) | 29.9 | 30 ms |
| Mint agent ↔ PC-LABORATORIO viewer, test server on the Mac | direct IPv4 (LAN) | 29.7 | 16 ms |
| The same with `NUVIRA_FORCE_RELAY=1` | relay | 29.3 | 40 ms |

The direct path over IPv6 between two networks was seen on 2026-11-07, between the Mac mini and the OVH VPS ([33-macos.md](33-macos.md)).

## Steps

| # | Step | Status |
|---|---|---|
| 1 | Go server: QUIC signalling, registration with signature, SQLite, 9-digit IDs. Agent: `--server`, prints ID and password | ✅ |
| 2 | Connection request, UDP relay, viewer connecting by ID with verified keys, session password | ✅ |
| 3 | Automated tests through the server in a LAN (`NUVIRA_VIA_SERVER=1`, `NUVIRA_FORCE_RELAY=1`) | ✅ |
| 4 | Docker image (`ghcr.io/nuvira-hub/nuvira-server`, GitHub Actions) | ✅ image; publishing on the Internet postponed |
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

### Measured again with 0.12.8 (2026-11-01)

NUC (agent, Quick Sync, full-screen animation) → laptop (viewer), local `nuvira-server` with `NUVIRA_RELAY_IMPAIR`, relay forced. With the Windows timer fix the encoder gives about 50 fps instead of 32, and the numbers of 2026-10-26 above could not be reproduced any more, not even with the 0.12.1 agent (built from its tag and run in the same conditions): on `delay=40ms,loss=2%,rate=2mbit` both gave a 95th percentile of about 1 s.

Isolating one cause at a time (30 s each, two runs):

| Simulated network | fps | p50 | p95 | Encoder restarts |
|---|---|---|---|---|
| 40 ms, 2 Mbit/s, adaptive bitrate | 36–43 | 91–101 ms | 990–1030 ms | 4–5 |
| 40 ms, 2% loss, no rate limit | 33–47 | 74–122 ms | 168–239 ms | 4 |
| 40 ms, 2 Mbit/s, fixed 1.5 Mbit/s | 56–57 | 76 ms | 93 ms | 0 |

The long delays came from the controller, not from the link: it raised the bitrate from 4 to 5.3 Mbit/s while a nearly still screen sent 0.3 Mbit/s (the animation then filled the 2 Mbit/s link: 1.7 s of queue), and after every decrease it climbed back past the link's rate (1.76 → 2.03 → 2.33 → 2.68 Mbit/s), restarting the encoder each time. Now it rises only while at least half of the bitrate is sent, and for 30 s after a congestion at most to 90% of the rate that was being sent then.

| Simulated network (45 s) | Before | After |
|---|---|---|
| 40 ms, 2 Mbit/s | 36–43 fps, p50 91–101 ms, p95 990–1030 ms | 52–53 fps, p50 81–84 ms, p95 148–193 ms |
| 40 ms, 2% loss, 2 Mbit/s | 44–49 fps, p50 124–146 ms, p95 790–1000 ms | 51 fps, p50 91–98 ms, p95 235–306 ms |
| 25 ms, 1% loss, 5 Mbit/s | 49 fps, p50 67 ms, p95 224–319 ms | 53 fps, p50 67–71 ms, p95 140–161 ms |
| Clean | 54 fps, 28 / 38 ms | 54 fps, 26 / 38 ms |

What is left on lossy links comes mostly from losses: the video goes on one reliable QUIC stream, so a lost packet holds the frames after it until it is retransmitted (at least one round trip).

### Prototype: video as datagrams with error correction (2026-11-01)

Opt-in, off by default: `NUVIRA_VIDEO_FEC=1` on the viewer (it asks in `Hello.video_datagrams`, the agent agrees if the connection has QUIC datagrams), `NUVIRA_FEC_RATIO` on the agent (parity shards per data shard, default 0.2). Each frame is cut into datagram-sized shards and every block of up to 32 gets Reed-Solomon parity (`client/transport/src/fec.rs`): any `k` of the `k + m` shards rebuild it with no retransmission. A frame that cannot be rebuilt is dropped and so are the next ones, up to a keyframe the viewer asks for. The adaptive bitrate counts the parity: it is what goes on the network, and the encoder gets `bitrate / (1 + ratio)`.

Same setup as above (relay, 45 s):

| Simulated network | Reliable stream | Datagrams + 20% parity | Frames not rebuilt |
|---|---|---|---|
| 40 ms, 2% loss, no rate limit | 33–47 fps, p95 168–239 ms | 53 fps, p50 68 ms, p95 79 ms | 1 |
| 25 ms, 1% loss, 5 Mbit/s | 53 fps, p50 67–71 ms, p95 140–161 ms | 46–53 fps, p50 60–64 ms, p95 105–145 ms | 5–35 |
| 40 ms, 2% loss, 2 Mbit/s | 51 fps, p50 91–98 ms, p95 235–306 ms | 47–48 fps, p50 74–85 ms, p95 130–155 ms | 23–32 |
| 40 ms, 2 Mbit/s, no random loss | 52–53 fps, p95 148–193 ms | 45.5 fps, p95 234 ms | 35 |
| Clean | 54 fps, 26 / 38 ms | 53 fps, 27 / 40 ms | 0 |

With random losses alone it does what it is for. On a saturated link it does not yet: the losses there come from the full queue, not at random. The session starts at 4 Mbit/s on a 2 Mbit/s link and the controller needs about 2 s to come down; BBR by design does not slow down on losses; so the queue overflows, whole runs of shards are lost, and each lost frame freezes the image until a keyframe (more parity, 40%, made it worse there: 39 frames lost; with random losses only it lost none). The latency percentiles do not show freezes: the number of frames not rebuilt does.

Then (same evening):
- **Freezes in the statistics**: the viewer counts pauses over 200 ms between frames shown (`freezes`, `freeze_ms` in `--stats-file`, each logged at debug level). With the tests' animation each is a visible freeze.
- **Losses in the feedback**: the viewer reports shards expected and lost and frames not rebuilt (`Feedback.shards`, `shards_lost`, `frames_lost`), on their own every 250 ms while no frame is shown. Frames lost lower the bitrate at once, but only with a queue building up (≥ 30 ms): lowering it for random losses took a link with 2% random loss from 3.4 to 0.8 Mbit/s.
- **Parity**: 0.1 + 3 × the share of shards lost (0.1–0.5), at least 2 shards per block (with 1, small frames of 2–4 shards were lost about once in 400 frames at 2% loss), keyframes twice as much. The encoder's share follows the overhead actually measured.

| Relay, 45 s | fps | p50 | p95 | Freezes | Frames not rebuilt |
|---|---|---|---|---|---|
| 40 ms, 2% loss, datagrams | 52 | 67 ms | 140 ms | 5, 3.5 s | 3 |
| 40 ms, 2% loss, 2 Mbit/s, datagrams | 50 | 101 ms | 186 ms | 7, 4.7 s | 0 |
| 40 ms, 2 Mbit/s, datagrams | 52 | 78 ms | 140 ms | 5, 3.4 s | 5 |
| Clean, datagrams | 52 | 25 ms | 36 ms | 5, 3.6 s | 0 |
| Clean, reliable stream | 53 | 26 ms | 40 ms | 3, 2.8 s | – |

**Most freezes are not losses: they are encoder restarts, in both modes.** Restarting Quick Sync takes 0.8–1.3 s, not the ~0.3 s assumed, and the image stops meanwhile: in a clean session, the switch to the viewer's window size (1.3 s → a 1005 ms freeze) and the first two bitrate rises (0.77 s → 792 ms, 0.91 s → 934 ms). Every bitrate change on an unstable link is a freeze of about a second.

Opening a session (debug timings): `ActivateObject` 440–880 ms, handing over the GPU's DXGI manager 150–250 ms, the types ~50 ms. Now a bitrate or size change keeps the encoder and its GPU link: flush, end of streaming, types set again (cleared first for another size, which Quick Sync otherwise refuses with E_FAIL), converter remade if the size changed, streaming again with a forced keyframe (`Session::reconfigure`, falling back to a new session on error). It takes 1–4 ms for a bitrate, ~19 ms for a size, and the bitrate is applied (1.6 Mbit/s asked, 1.5–1.6 sent; changing it while streaming is still ignored).

| Relay, 45 s | Before | With reconfiguration |
|---|---|---|
| Clean, reliable stream | 53 fps, 3 freezes, 2.8 s | 57 fps, 0 freezes |
| Clean, datagrams | 52 fps, 5 freezes, 3.6 s | 57 fps, 0 freezes |
| 25 ms, 1% loss, 5 Mbit/s, stream | 53 fps, p95 140–161 ms | 53 fps, p95 164 ms, 0 freezes |
| 25 ms, 1% loss, 5 Mbit/s, datagrams | 46–53 fps, p95 105–145 ms | 57 fps, p95 118 ms, 0 freezes |
| 40 ms, 2% loss, datagrams | 52 fps, p95 140 ms, 5 freezes | 57 fps, p95 82 ms, 0 freezes |
| 40 ms, 2% loss, 2 Mbit/s, stream | – | 54 fps, p95 330 ms, 6 freezes, 2.3 s |
| 40 ms, 2% loss, 2 Mbit/s, datagrams | 50 fps, 7 freezes, 4.7 s | 55 fps, p95 246 ms, 3 freezes, 1.05 s |

The freezes left are on the 2 Mbit/s link and come from the start: 4 Mbit/s on a 2 Mbit/s link makes a queue of over a second before the bitrate comes down.

**Start on a slow link.** The first decrease was held at half the bitrate (4 → 2 Mbit/s, still the link's rate) and the queue then took 6–10 s to empty. Now a decrease goes below half when at least 40% of the bitrate is sent (the rate sent then measures the link), and it also takes off the queue in seconds (up to half) so that the queue empties: 4 → 0.87 Mbit/s with a 0.8 s queue, back to 51 ms of network latency 3 s later, then up by 15% a second. The freezes of the start now end within about 6.5 s of the session start (3–4 short ones while the first second of overshoot drains) instead of about 10 s; single 45 s runs vary a lot on this link (stream: 5 and 12 freezes in two runs, the second with a burst at 38–42 s).

**Keyframes asked by the viewer: at most one a second** at the agent, and the viewer asks again every second (was 500 ms). With datagrams on the 2 Mbit/s link every lost frame asked for one, each large and with double parity, and they caused more losses: 2 Mbit/s went out while the bitrate had come down to 0.5.

| Relay, 45 s, datagrams | fps | p50 | p95 | Freezes |
|---|---|---|---|---|
| 40 ms, 2 Mbit/s | 56–57 | 132–181 ms | 261–262 ms | 2–4, 0.6–1.4 s |
| 40 ms, 2% loss, 2 Mbit/s | 55–56 | 90–91 ms | 222–249 ms | 2–4, 1.2–2.2 s |
| 40 ms, 2% loss | 57 | 66 ms | 77 ms | 0 |
| 25 ms, 1% loss, 5 Mbit/s | 56 | 58 ms | 135 ms | 1, 1.0 s |

Where things stand: the datagrams are clearly better with random losses (Wi-Fi, mobile) and not yet on a saturated link, where the median stays high (a standing queue, probably in the datagram send buffer, 1 MB by default) and a lost frame still costs a keyframe. Further work there: a smaller datagram send buffer, and recovery without keyframes (intra refresh, or the viewer telling which frame it has last).

Still open for the datagrams: a start that does not overshoot an unknown link.

### Datagrams on a saturated link (2026-11-02)

NUC (agent) → laptop (viewer), relay with `delay=40ms,rate=2mbit`, 45 s, datagrams.

**Smaller datagram send buffer: no effect.** quinn keeps up to 1 MB of datagrams waiting for the congestion window (4 s at 2 Mbit/s), the suspected standing queue. With 1 MB, 64 kB and 16 kB the median stayed at 104–153 ms: the queue is on the link, not in the buffer.

**The queue came from the parity.** The agent's logs showed 1.8–2.0 Mbit/s sent on the 2 Mbit/s link and 0.9 Mbit/s of video: frames of ~2 kB are two shards, and at least two parity shards doubled them. Now one parity shard is enough while no losses are measured (parity asked below 0.12, i.e. under 0.7% of shards lost), two as before with losses:

| 2 Mbit/s, datagrams | Before | One parity shard without losses |
|---|---|---|
| Median latency | 125–170 ms | 76–83 ms |
| 95th percentile | 251–290 ms | 222–259 ms |
| Freezes | 2–4 | 1–5 |
| 2% loss, no rate limit | 67 / 78 ms, 0 freezes | 67 / 78 ms, 0 freezes |

The 95th percentile and the freezes come from the first ~10 s (4 Mbit/s asked of a 2 Mbit/s link); afterwards the network latency stays at 46–51 ms. Tried and not kept, no measurable difference: bitrate ceiling after a congestion at 80% instead of 90%, latency baseline over 30 s instead of 10; starting with 0.1 parity instead of 0.2 gave more freezes at the start.

**Recovery without keyframes: long-term references (LTR), not adopted.** Every H.264 hardware encoder here offers them through Media Foundation (`CODECAPI_AVEncVideoLTRBufferControl`, `MarkLTRFrame`, `UseLTRFrame`; gradual intra refresh everywhere but the HD 2500, example `codecapi`). The encoder keeps frames as references; after a loss the next frame refers only to one the viewer has. Example `ltrtest` (frame 30 kept, frames 60–62 dropped, frame 63 from the reference, PSNR of the luminance against the original):

| Encoder | Keyframe | Recovery frame (reference 0.5 s old) | Hardware decoder after recovery |
|---|---|---|---|
| Quick Sync (NUC, laptop) | 14.9 kB | 2.0 kB | 68 dB, as without loss (21–27 without recovery) |
| NVIDIA RTX 4060 Ti | 9.0 kB | 2.4 kB | 69 dB |
| HD 2500 (PC-LABORATORIO) | 8.2 kB | 21 kB | correct, but larger than a keyframe |

OpenH264, the viewer's default decoder, decodes streams with long-term references wrongly even without losses (22 dB on Quick Sync; the NVIDIA stream not at all) and refuses every frame after a gap, also with error concealment. So it would work only with the hardware decoder (`NUVIRA_DECODER=hardware`, not the default: see `mfdec`). Built and measured live that way (agent marks a reference every 0.5 s, the viewer reports the newest it has and asks to start from it): recoveries came from the reference, but each freeze still lasted 260–370 ms, the time to notice the loss and the round trip over a queued link; a keyframe's extra size costs only ~60 ms at 2 Mbit/s. Total frozen time 0.9–1.7 s against 2.2–8.2 s in two runs each, the count unchanged. Not adopted: small gain, only with the hardware decoder, and a viewer falling back to OpenH264 during a session would show a broken image without any error. The encoder keeps the support (`EncoderSettings::ltr`, `VideoEncoder::mark_ltr` / `use_ltr`) for when the viewer draws the hardware decoder's output directly.

## What changes over the Internet

The OPNsense configuration (forwarding the server's UDP port) only makes the server reachable; the software still has to be checked under conditions a LAN does not present:

- **Symmetric NATs and CGNAT** (mobile networks): the direct path may fail and the session stays on the relay; how often remains to be measured.
- **Hairpin**: two devices behind the same NAT reaching the server through the public IP. Solved with local candidates (above); verified in a LAN by simulating an unreachable public address.
- **Networks that block UDP** (companies, hotels): WebSocket fallback (above). To get through corporate proxies and firewalls it must be published as `wss://` on port 443 behind the reverse proxy.
- **Packet loss, jitter, RTT of 30–100 ms**: handled with adaptive bitrate and BBR (section above), verified in simulation; the real test remains.
- Smaller **MTUs** (VPN, PPPoE): QUIC handles them, to be verified.

Some of these points can be simulated in a LAN (artificial loss and delay) before the real test.
