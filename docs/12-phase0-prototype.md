# NuviraHub — Phase 0: Windows prototype in a LAN

[← Index](README.md)

Goal of the phase: capture, encoding with the graphics card and display in a LAN, with latency measured.

## Status

| Part | Module | Status |
|---|---|---|
| Protobuf protocol + framing | `client/proto` | ✅ |
| DXGI Desktop Duplication screen capture | `client/capture` | ✅ tested on the real screen |
| Synthetic source for tests | `client/capture` | ✅ |
| BGRA → I420/NV12 conversion (integer, multi-core) | `client/codec` | ✅ ~3–4 ms at 1080p |
| Media Foundation hardware encoder (Intel Quick Sync) | `client/codec` | ✅ |
| OpenH264 software encoder (fallback) | `client/codec` | ✅ |
| QUIC transport with certificate fingerprint | `client/transport` | ✅ |
| Mouse and keyboard (SendInput, W3C keys) | `client/input` | ✅ |
| wgpu viewer (Direct3D 12, YUV → RGB in the shader) | `client/viewer` | ✅ |
| Viewer drawing on the CPU for OpenGL-only cards | `client/viewer` | ✅ 6 ms at 1080p on an HD 2500 |
| Capture and encoding on two threads | `client/agent` | ✅ |
| Image always on the GPU: capture → Video Processor (NV12) → Quick Sync | `client/capture`, `client/codec` | ✅ conversion ~1 ms |
| Automatic encoder choice (test at start-up) | `client/codec` | ✅ |
| Automated tests between two PCs | `tools/lan-test.sh` | ✅ |
| Hardware decoding in the viewer | — | to do |

## How to try it

Build requirements: stable Rust, Visual Studio Build Tools (C++) and, recommended, **NASM** in the `PATH` during the build (OpenH264 goes from 19 to 9 ms per frame). The NASM installer puts it in `%LOCALAPPDATA%\bin\NASM` without adding it to the `PATH`.

```powershell
cargo build --release
```

On the PC to control:

```powershell
target\release\nuvira-agent.exe                # primary screen, automatic encoder
target\release\nuvira-agent.exe --list-displays
target\release\nuvira-agent.exe --display 1 --encoder software
target\release\nuvira-agent.exe --synthetic 1920x1080@60   # generated image, no screen
```

The agent prints its certificate fingerprint. On the controlling PC:

```powershell
target\release\nuvira-viewer.exe 192.168.1.20 --fingerprint <fingerprint>
```

The window title shows fps, capture→display latency, RTT, bandwidth and encoding and decoding times. The default port is **UDP 21500** (it must be opened in the agent's Windows firewall).

> This direct LAN mode has no authentication besides the fingerprint and one session at a time: trusted networks only. With a server see [13-phase1-network.md](13-phase1-network.md).

## Automated test between two PCs

`tools/lan-test.sh [seconds] [agent options]` starts the agent on this PC and the viewer on another PC over SSH (in the user's Windows session, through a scheduled task), then collects logs and JSON results in `target/lan-test/`. Example:

```bash
tools/lan-test.sh 15 --synthetic 1920x1080@60 --encoder software
NUVIRA_VIEWER_LOG="info,nuvira_viewer::render=trace" tools/lan-test.sh 10
```

Options used by the script: viewer `--duration`, `--stats-file`; agent `--once`, `--fingerprint-file`. Prerequisites: OpenSSH server on the remote PC with the key `~/.ssh/nuvira_test`, a firewall rule for `dist/nuvira-agent.exe` (UDP 21500, LAN only).

## LAN measurements with the real screen (2026-10-26, evening)

Agent on PC A (Intel Core i7-10710U, Intel UHD), viewer on PC B (drawing on the CPU), PC A's 1920x1080 screen with a full-screen animation (`NUVIRA_ANIMATE=1`), other remote control software closed.

| Encoder | fps | Latency p50 | p95 | Encoding | Bandwidth |
|---|---|---|---|---|---|
| Hardware, image always on the GPU | 32 | **38 ms** | 46 ms | 16 ms | 3.3 Mbit/s |
| Software (OpenH264 + NASM) | 40 | 55 ms | 70 ms | 25 ms | 1.2 Mbit/s |

Problems found and fixed in this series:
- **Lock of the shared device**: `AcquireNextFrame` held the D3D11 device lock for the whole wait (up to 100 ms), blocking conversion and encoding (up to 360 ms per frame). In GPU mode capture polls without waiting, every millisecond.
- **Fps limiter**: the 16.7 ms pause started from the end of encoding instead of its start.
- **Image ageing while waiting for the encoder**: now the agent waits for Quick Sync to be ready and only then takes the most recent image (latency from 53 to 38 ms).

Former hardware limit: Quick Sync took a new frame only ~31 ms after the previous one, hence ~32 fps. Cause (2026-10-31, example `hwpace`): the driver waits for ticks of the Windows timer, 15.6 ms by default, once before accepting a frame and once before returning it, whatever the resolution (same times from 640x360 to 2560x1440). With the timer at 1 ms (`timeBeginPeriod`, only while an encoder session is open, and the process exempt from Windows 11's timer throttling) the encoder alone goes from 32.8 to 93 fps and from 15.6 to 8.9 ms per frame, still with no frame held back. Without low-latency mode it also reached 65 fps, but returning every frame one call late: rejected. Between the two PCs (synthetic 1080p source): 24.5 → 47 fps shown, latency p50 48 → 45 ms, p95 83 → 69 ms. With a browser open on PC A (30% of the 3D engine) times get worse.

## LAN measurements, synthetic source (2026-10-26)

Agent on PC A (i7-10710U, Intel UHD), viewer on PC B (Intel HD Graphics 2500, OpenGL because it has no Direct3D 12), wired network, synthetic source 1920x1080@60, 15 s.

| Encoder | fps | Latency p50 | p95 | Encoding | Decoding | Drawing | Bandwidth |
|---|---|---|---|---|---|---|---|
| Hardware (Quick Sync) | 21 | 51 ms | 62 ms | 18 ms | 13 ms | 24 ms | 2.4 Mbit/s |
| Software (OpenH264 + NASM) | 27 | **39 ms** | 51 ms | 11 ms | 5 ms | 24 ms | 0.3 Mbit/s |

Problems that showed up only between two PCs, and were fixed:
- **Growing latency** (up to 2.5 s): the viewer showed all queued frames even when it was slower than the agent. Now it always shows the last decoded frame and drops the overtaken ones.
- **Slow drawing on OpenGL** (47 ms per frame): with `Fifo` it waited for vsync before and after every frame. Without `Mailbox`, `Immediate` is used (24 ms, some tearing possible).
- Cards without compute shaders: the viewer requests only the minimum limits (WebGL2).

## Local measurements (2026-10-26)

Test PC: Intel Core i7-10710U, Intel UHD Graphics, Windows 11. Agent and viewer **on the same PC** (localhost), therefore competing for CPU and GPU.

### Real screen

Primary 1920x1080 screen with the viewer window visible (so always changing), OpenH264 built with NASM.

| Encoder | fps | Capture→display latency | Encoding | Decoding |
|---|---|---|---|---|
| Hardware (Quick Sync) | 18–22 | **32–40 ms** | 18–22 ms | 7–10 ms |
| Software (OpenH264 + NASM) | 20–27 | **22–33 ms** | 11–19 ms | 4–5 ms |

### Synthetic source

1920x1080 image at 60 fps with a moving bar, OpenH264 without NASM.

| Encoder | fps | Capture→display latency | Encoding | Decoding (OpenH264) | Bandwidth |
|---|---|---|---|---|---|
| Hardware (Quick Sync through Media Foundation) | 20–23 | **36–54 ms** | ~20 ms (4 conversion + 7–20 encoder) | 14–25 ms | ~2.9 Mbit/s |
| Software (OpenH264) | 16–19 | 46–157 ms | 25–39 ms | 4–5 ms | ~0.2 Mbit/s |

Encoder benchmark (`cargo run --release -p nuvira-codec --example bench`), desktop-like 1080p image:

| Encoder | ms/frame |
|---|---|
| Quick Sync, steady state | ~15 (4 conversion + 11 encoding) |
| OpenH264, "camera" mode | 19 |
| OpenH264, "screen content" mode | 98 (discarded) |

Notes:
- On Windows wgpu uses **Direct3D 12**: the Intel Vulkan driver of this PC crashes at start-up, so Vulkan is excluded by default (`WGPU_BACKEND` still allows forcing it).
- With the screen locked capture is refused (`E_ACCESSDENIED`): it is the secure desktop, which only the system service can capture (phase 3, done: see [14-phase3-service.md](14-phase3-service.md)).
- OpenH264 without NASM: 19 ms per frame in the benchmark; with NASM: 9 ms.
- **QUIC and GSO on Windows**: sending several datagrams with a single call (segmentation offload) lost packets on loopback and the connection stopped receiving (about 4 sessions out of 10 without video). Tried again between two PCs (2026-11-01): 20 sessions out of 20 fine over Wi-Fi and cable, but no gain at our bitrates (8 sessions each to the laptop, median 42.9 fps and 34.9 ms without GSO, 40.6 fps and 36.8 ms with it). GSO stays disabled in `client/transport`.
- **Keyframes**: with Quick Sync in constant bitrate and low-latency mode a keyframe is about twice a normal frame (1080p synthetic source: 16–32 kB against ~14 kB), so there is no spike worth gradual intra refresh, which Media Foundation does not expose for Quick Sync anyway. Most unrequested keyframes come from encoder restarts when the adaptive bitrate changes (`RUST_LOG=nuvira_agent::pipeline=debug` logs each one).

## Next steps

Done: separate threads, zero-copy path, measurements between two PCs with the real screen, adaptive bitrate (phase 1).

1. ~~Quick Sync pacing~~: done (1 ms timer, above).
2. **Hardware decoding in the viewer**: Windows' H.264 decoder with DXVA works (`mfdec`, identical images, no frame late) but reading the image back costs more than OpenH264 (1080p: 9.7 against 4.5 ms on Intel UHD, 7.3 against 4.2 ms on HD 2500, 6 ms of it copy and map). Opt-in with `NUVIRA_DECODER=hardware`; useful only when the renderer draws the decoder's texture (D3D11 → shared texture → wgpu on D3D12).
3. ~~Encoding of changed areas only~~: H.264 encoders take whole frames, and unchanged areas already cost almost nothing (skipped macroblocks); a still screen is not encoded at all after the refinement passes. What saves pixels instead is **encoding at the size the viewer draws** (2026-10-31): the viewer sends `ViewSize` (its window in pixels, 0x0 in real-size view), and the agent reduces the image to fit, on the GPU in the same video-processor pass as the colour conversion, on the CPU by averaging pixels (`convert::scale_bgra`); a new size applies after 300 ms without changes. 4K synthetic source into a 1280x720 window (NUC → laptop): 23.4 → 40.1 fps, latency p50 85 → 39 ms, p95 172 → 54 ms, viewer decoding 28 → 5.6 ms. Example `gpuscale` checks the GPU reduction against the CPU one.
