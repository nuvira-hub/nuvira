# Remote audio

[← Index](README.md)

What the controlled computer plays (a video, a notification, a call) comes out of the technician's speakers. The sound is on from the start of every session; the speaker button in the viewer's toolbar mutes it, and then the computer stops capturing it and nothing goes over the network.

## Where it works

| System | Sends its sound (controlled) | Plays the sound (technician) |
|---|---|---|
| Windows | yes: WASAPI loopback of the default output device | yes |
| macOS | not yet (ScreenCaptureKit audio, planned) | yes |
| Linux | not yet (PipeWire / PulseAudio monitor, planned) | yes (ALSA, also through PipeWire and PulseAudio) |
| Web client (Pro) | — | not yet |

Session recordings stay video only.

## How

- **Capture** (`client/audio/src/capture.rs`): `cpal` opens the default output device as an input, which on Windows is WASAPI loopback: the mix sent to the speakers, before the volume. The device's format (often 48 kHz float, sometimes 44.1 kHz or more channels) is converted to 48 kHz stereo (`rubato`). Windows's buffer is opened at 100 ms instead of its default of about 10 ms: on a busy computer the capture thread can be late, and with the small buffer Windows drops the sound meanwhile (an audible gap). It adds no delay, the data is read as soon as it is there. The gaps Windows still reports are counted and logged every 10 s; the player logs its losses, underruns and trims the same way. WASAPI delivers nothing while nothing plays: no packets then.
- **Codec** (`client/audio/src/opus.rs`): Opus at 48 kHz stereo, 20 ms packets, 96 kbit/s, with in-band redundancy (FEC) for one lost packet. The library is `unsafe-libopus`, libopus translated to Rust: no C compiler or cmake in the build.
- **Transport** (`client/audio/src/packet.rs`): each packet is one QUIC datagram, `0xA0, seq u32, Opus packet`: a lost one is not retransmitted (it would arrive too late), and it does not hold up anything else. The first byte tells it apart from the video's datagrams (`nuvira_transport::fec`, version 1). Over the WebSocket fallback the datagrams travel inside the tunnel like the rest.
- **Playback** (`client/audio/src/player.rs`): a decoding thread fills a buffer that the output device's callback empties. It starts playing once 60 ms are there, plays silence and waits to refill when it runs dry, and drops the oldest audio beyond 250 ms, so the delay stays short. A lost packet is rebuilt from the next one's redundancy; up to 5 in a row are made up by Opus's concealment, beyond that it is silence (the sound stopped).

## Protocol

- `Hello.audio` (field 20): from the viewer, it plays the sound (it has an output device, the connection has datagrams, it is not a terminal-only session); from the agent, it will send it. Audio flows only when both set it, so older viewers never receive an audio datagram, and older agents never send one.
- `SetAudio { on }` (control message 31), from the viewer: the mute button. Off stops the capture at the agent; on starts it again.

## Tests

- `cargo test -p nuvira-audio`: a 1 kHz tone through Opus comes out at 1 kHz; datagram format; rate and channel conversion.
- Live, 2026-11-12, with a test tone (440 Hz, 1 kHz, 2.5 kHz, 3 s each) played at the agent and the viewer writing what it receives to a WAV file (`NUVIRA_AUDIO_DUMP=file.wav`: nothing is played then, so a test on one computer does not capture its own playback):
  - NUC to NUC, direct: 9.00 s received, frequencies exact, same level, no gaps.
  - PC-LABORATORIO to the NUC through the NAS's server: 455 packets, none lost, the three tones complete.
  - Through a relay with `NUVIRA_RELAY_IMPAIR="delay=30ms,jitter=15ms,loss=5%"`: 25 of 451 packets lost, all concealed or rebuilt, 9.00 s with no gaps.
  - Mute and unmute with the toolbar button (pressed through UI Automation): the capture stops at once and nothing is received while muted; after unmuting the next tone arrives complete.
- Listening, 2026-11-12, PC-LABORATORIO (an old PC) to the NUC, a YouTube video, scrolling the screen meanwhile: good sound, video and sound together. With cpal's default buffer Windows dropped sound 5 to 10 times every 10 s (small tears); with the 100 ms buffer 0 to 2 times, rare tears on that PC only at full load. The viewer lost no packet and never ran dry.

## To do

- macOS and Linux capture.
- A rule of the Pro server (`Policy.no_audio`) and a switch for the person at the PC, like the other permissions ([32](32-session-permissions.md)).
- Audio in session recordings and in the web client.
