# NuviraHub — Statistics log

[← Index](README.md)

An optional file of numbers about a session, written by the agent and by the
viewer while it runs: frames, latency, bitrate, losses, QUIC state, CPU and
memory of the process, one line per interval. It is meant for measurements on
real machines (the same figures the benchmark suite measures in loopback) and
for diagnosing a slow session. **It is off unless an environment variable turns
it on**: there is no setting in the app, no command-line option, no change to
the window, and nothing is sent to the other side or to the server.

## Turning it on and off

| Variable | Value | Effect |
|---|---|---|
| `NUVIRA_STATS` | `1` (or `on`, `true`, `yes`) | files in `<data folder>/stats/` (the folder of `NUVIRA_DATA_DIR` when set; otherwise `%APPDATA%\nuvira`, `~/Library/Application Support/nuvira`, `~/.config/nuvira`) |
| | a folder | files in that folder (created if missing) |
| | unset, empty, `0`, `off`, `false`, `no` | off (the default) |
| `NUVIRA_STATS_INTERVAL_MS` | milliseconds | interval between two lines; default 1000, at least 250 |
| `NUVIRA_RELAY_STATS` (server) | a duration (`10s`, `1m`) or seconds; at least 1 s | a `relay stats` line in the server's log every interval |

The variables are read at the start of each session, by the process that runs
it: `nuvira agent` / `nuvira-agent` and `nuvira viewer` / `nuvira-viewer`, the
app's own sessions and the service's agent (they inherit the environment of
the process that starts them; for the Windows service or a systemd unit the
variable has to be in the service's environment). Removing the variable turns
it off from the next session. Terminal-only sessions and the guests of a
shared session write nothing.

With it off the code does not even touch its counters (the hot path tests one
`Option` per frame). With it on, the hot path adds a few relaxed atomic
operations per frame (about 25 ns per duration recorded) and never allocates;
a separate thread at low priority (nice 10 on Linux, `THREAD_PRIORITY_LOWEST`
on Windows; normal priority on macOS) formats and writes one line per
interval.

## Files

One file per session and side:
`<side>-<yyyymmdd-hhmmss>-<session>.jsonl` (UTC time of the start; `side` is
`agent` or `viewer`; `session` is the session id given by nuvira-server, the
same on both sides, or a local id `local…` for LAN sessions without a server
and for switch sides). A file that reaches 50 MB is closed and the session goes
on in `<…>.2.jsonl`, `<…>.3.jsonl`, each starting with the header again. At
most 20 statistics files are kept in the folder: when a new one is created the
oldest (by modification time) are deleted. Other files in the folder are never
touched.

The format is JSON Lines (schema `nuvira-stats/1`): a header, one line per
interval, a summary at the end. A value a side cannot know is `null`, never an
invented 0 (for example `fec` with the video on its stream, `abr_target_kbps`
with a fixed bitrate, `feedback` in an interval without viewer reports).

A session through the relay of a local nuvira-server on MSI-UBUNTU (2026-11-17; the same
session id on both sides; lines 1, 9 and the last of each file):

```json
{"type":"header","schema":"nuvira-stats/1","side":"agent","app_version":"0.28.0","os":"linux","session":"0e945b91dbb7c997","path":"relay","encoder":"openh264","plan":"Software","width":1920,"height":1080,"fps_max":60,"interval_ms":1000,"part":1}
{"type":"interval","t_ms":8000,"side":"agent","frames_captured":59,"frames_sent":60,"frames_skipped":1,"keyframes":0,"keyframe_requests":0,"capture_ms":{"p50":1.093,"p95":1.514,"max":1.514},"encode_ms":{"p50":5.175,"p95":6.153,"max":6.153},"send_blocked_ms":0.418,"bytes_sent":74483,"kbps_sent":597,"abr_target_kbps":4000,"fec_ratio":null,"feedback":{"reports":4,"latency_p50_ms":11,"latency_min_ms":10,"transit_p50_ms":0,"frames_lost":null,"shards_lost":null},"quic":{"rtt_ms":0.306,"cwnd":943821,"sent_packets":85,"lost_packets":0,"congestion_events":0,"udp_tx_bytes":79306},"cpu_ms":1520.255,"rss_mb":82.1,"threads":13}
{"type":"summary","seconds":14.459,"frames":858,"fps":59.34,"latency_ms":null,"kbps":507,"cpu_ms":22476.432,"rss_mb_max":82.3,"exit_reason":"stream_stopped_by_peer"}
{"type":"header","schema":"nuvira-stats/1","side":"viewer","app_version":"0.28.0","os":"linux","session":"0e945b91dbb7c997","path":"relay","decoder":"openh264","width":1920,"height":1080,"interval_ms":1000,"part":1}
{"type":"interval","t_ms":8000,"side":"viewer","frames_received":60,"frames_decoded":60,"frames_shown":60,"frames_dropped_slot":0,"latency_ms":{"p50":11.437,"p95":25.859,"p99":26.576,"max":26.576},"transit_ms":{"p50":0.347,"p95":0.442},"decode_ms":{"p50":0.796,"p95":0.992},"render_ms":{"p50":1.547,"p95":1.932},"decode_queue_wait_ms":0.304,"freezes":0,"freeze_ms":0,"rtt_ms":0.504,"kbps_received":590,"fec":null,"quic":{"rtt_ms":0.353,"lost_packets":0},"cpu_ms":467.573,"rss_mb":204,"threads":39}
{"type":"summary","seconds":14.507,"frames":858,"fps":59.144,"latency_ms":{"p50":17.386,"p95":25.92,"max":82.259},"kbps":595,"cpu_ms":6889.059,"rss_mb_max":204.1,"exit_reason":null}
```


### Header

`side`, `app_version`, `os`, `session`, `path` (`direct`, `relay`,
`websocket`, or `nested` for switch sides), `interval_ms`, `part` (1, 2, … of
the rotated file), then per side:

- agent: `encoder` (name of the encoder in use at the start), `plan`
  (`Software`, `Hardware`, `HardwareGpu`), `width`/`height` of the captured
  screen at the start, `fps_max`;
- viewer: `decoder` (at the start; a later fallback to OpenH264 is in the log
  as before), `width`/`height` of the first decoded picture.

The header is written with the first interval line, when encoder and decoder
are known.

### Interval (all counts are of that interval only)

Agent:

| Field | Meaning |
|---|---|
| `frames_captured`, `frames_sent`, `frames_skipped` | pictures from the capture, frames handed to the network, captured pictures overwritten before the encoder took them |
| `keyframes`, `keyframe_requests` | keyframes encoded; `RequestKeyframe` messages from the viewer (also those ignored by the one-per-second limit) |
| `capture_ms` | from the moment the OS handed the picture over (DXGI, PipeWire, ScreenCaptureKit, X11, the time stamp of the capture) to its copy in the encoder's slot: the capture's own copying and mapping, not the wait for a screen change |
| `encode_ms` | encoding of one frame (`VideoFrame.encode_us`) |
| `send_blocked_ms` | time the encoding thread waited for the network task to take a frame (single-frame queue) |
| `bytes_sent`, `kbps_sent` | video bytes on the network (parity shards included with datagrams) |
| `abr_target_kbps` | target of the adaptive bitrate (`null` with `--fixed-bitrate`) |
| `fec_ratio` | parity per data shard with the video as datagrams, else `null` |
| `feedback` | the viewer's `Feedback` reports of the interval: count, the last median and minimum latency and network transit, the frames and shards lost (summed; `null` in stream mode) |
| `quic` | `quinn` connection statistics: current RTT and congestion window; packets sent and lost, congestion events and UDP bytes sent in the interval |

Viewer:

| Field | Meaning |
|---|---|
| `frames_received`, `frames_decoded`, `frames_shown`, `frames_dropped_slot` | complete frames from the network, decoded, drawn, decoded but overtaken by a newer one before being drawn |
| `latency_ms` | capture (agent) to drawn (here), with the clock offset measured by the pings; `null` until the offset is known |
| `transit_ms` | end of encoding (agent) to arrival of the whole frame (here) |
| `decode_ms`, `render_ms` | decoding; upload and drawing in the window |
| `decode_queue_wait_ms` | time the network task waited because the decoding queue (4 frames) was full |
| `freezes`, `freeze_ms` | pauses between two frames drawn longer than 200 ms (as the `--stats-file` report) |
| `rtt_ms` | last ping round trip of the control channel; `quic.rtt_ms` is QUIC's own estimate |
| `kbps_received` | video bytes of the frames received |
| `fec` | with the video as datagrams: shards expected, shards lost, frames completed thanks to parity (`frames_rebuilt`: a data shard really lost, counted like the shards once the frame can get no more of them, so a data shard only reordered behind a parity shard does not count), frames lost (a frame whose header announces more bytes than its shards hold is dropped and counts here); else `null` |

Both sides: `cpu_ms` (CPU time of the whole process in the interval, user +
system), `rss_mb` (resident memory, Windows working set), `threads`. On
Windows the CPU time advances in steps of the scheduler's clock (about 15.6
ms) and the thread count is read every 30 s (reading it means walking the
system's whole process list: 7.6 ms on an i7-10710U), the last value is
repeated in between.

Durations are kept in 64 buckets per interval (below 0.1 ms, 62 buckets
log-spaced from 0.1 ms to 10 s, 10 s and more): a percentile is interpolated
inside its bucket (each about 20% wide) and never above the maximum, which is
exact.

### Summary

`seconds`, `frames` (sent by the agent, drawn by the viewer), `fps`,
`latency_ms` (viewer: p50/p95/max over the whole session; agent: `null`, it
only gets the viewer's medians), `kbps`, `cpu_ms` (whole session),
`rss_mb_max`, `exit_reason`: `null` when the session ended without error,
otherwise one word: `closed_by_peer`, `timed_out`, `connection_closed`,
`closed_locally`, `reset`, `transport_error`, `cids_exhausted` (the QUIC
connection), `stream_stopped_by_peer`, `stream_reset_by_peer`, `stream_error`
(a stream of it) or `error` (anything else; the error itself is in the normal
log, not here, as it may contain an address). When the viewer closes its window
the agent usually reads `closed_by_peer` or `stream_stopped_by_peer` (the
viewer stopped the video stream first): both are a normal end.

## Relay statistics (server)

With `NUVIRA_RELAY_STATS=5s` nuvira-server adds a line to its log every 5 s
(here one relayed session with the video as datagrams, PC-LABORATORIO):

```
time=2026-11-17T21:20:10.304+01:00 level=INFO msg="relay stats" interval_s=5 packets=2670 bytes=2222630 mbit=3.556 queue_drops=0 queue_len=0 sessions_open=1 sessions_waiting=0
```

`packets` and `bytes` forwarded between the sides of relayed sessions in the
interval, the rate, `queue_drops` (datagrams dropped because the receive queue
of 4096 datagrams was full: before this they were silent), the queue's length
at that moment and the relayed sessions open and waiting for their second
side. The counters are always kept (one addition per forwarded datagram, under
the lock the forwarding already takes); only the line is optional. A value
that is not a duration, or shorter than 1 s, stops the server at the start
with an error (as `NUVIRA_RELAY_IMPAIR`).

## Privacy

What is written is numbers, the session id that nuvira-server gives to both
sides, the path type, the encoder/decoder names, the OS and the app version.
There are **no IP addresses, no device names or IDs, no file names, no
keystrokes, no clipboard and no screen content**; error texts (which can hold
an address) are replaced by one word. The files stay on the computer that
writes them; nothing is uploaded. Under the GDPR a session id together with
the server's session log can still relate a file to a session and so to the
people in it: a Pro customer that turns the log on for its technicians should
treat the files like its other session logs (retention, access) and say so in
its privacy notice. Deleting the folder removes everything.

## Not done yet

- **Web client** (Pro): a `?stats=1` page parameter that writes the same lines
  to the browser console, with a download from the console only. Later.
- **`nuvira-bench import-stats`**: reading these files into the benchmark's
  tables (`bench-compare` on real sessions). Later.

## Tests of 2026-11-17

Unit tests (`cargo test --release -p nuvira-transport -p nuvira-agent -p nuvira-viewer`, Windows
and Linux): histogram buckets and percentiles, JSON values and `null`s, environment parsing,
header/interval/summary lines, a dropped recorder still ending its file, rotation (every part
starts with the header) and retention (20 files, other files kept), session ids, exit reasons,
the process sample, the agent's feedback sums, `fec::Receiver::take_rebuilt`. Go:
`relay` tests for the counters, the interval parsing and the log line (no address in it).

- **Linux, MSI-UBUNTU** (Ubuntu 26.10, i7-4770S; the viewer window through Xwayland): real
  agent (`--synthetic 1920x1080@60`) and viewer, 15 s each: stats off (no `stats` folder),
  `NUVIRA_STATS=1` (files in `$NUVIRA_DATA_DIR/stats`), datagrams + FEC at 500 ms
  (`fec` filled), through a local nuvira-server with the relay forced (`path` `relay`, the
  same session id on both sides, `relay stats` every 2 s). The viewer's summary agrees with
  its `--stats-file` report: 876 / 876 frames, latency p50 17.6 / 17.2 ms, p95 25.7 / 25.6 ms
  (relay: 858 / 858, 17.4 / 17.2, 25.9 / 25.9); the agent's frames sent = frames shown +
  dropped.
- **Windows, PC-LABORATORIO** (Windows 10, Ivy Bridge, Media Foundation): agent and viewer
  window on the same PC in loopback, baseline-pre-review binaries and this branch with the
  log off and on, 3 sessions each alternated: no file with it off; agent CPU per frame
  30.69 / 30.69 / 30.80 ms, viewer 17.64 / 17.58 / 17.39 ms, fps 44.2 / 44.0 / 43.9, latency
  p50 45.0 / 45.2 / 45.2 ms (medians; the run-to-run spread is about 3%). A relayed session
  through a local server with `NUVIRA_RELAY_IMPAIR=loss=3%` and FEC: 8385 shards, 260 lost
  (3.1%; the agent's feedback sum is the same 260), 156 frames rebuilt thanks to parity, 4
  lost, no freeze.
- **Overhead** (benchmark B-MACRO-03, MSI-UBUNTU, `bench-compare`, full tier): see
  nuvira-pro/docs/review/baseline/stats-gap.md. With the log on, CPU ms per frame of the agent
  changed by -2.8 to +0.4% (within noise), allocations per frame of the agent by -0.07 to +0.01 (none on
  the hot path).
- **macOS: NOT TRIED** (no Mac available). The macOS process sample (`getrusage`,
  `proc_pidinfo`) was only type-checked for aarch64-apple-darwin.
