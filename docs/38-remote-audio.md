# Remote audio

[← Index](README.md)

What the controlled computer plays (a video, a notification, a call) comes out of the technician's speakers. The sound is on from the start of every session; the speaker button in the viewer's toolbar mutes it, and then the computer stops capturing it and nothing goes over the network.

## Where it works

| System | Sends its sound (controlled) | Plays the sound (technician) |
|---|---|---|
| Windows | yes: WASAPI loopback of the default output device | yes |
| macOS | yes: ScreenCaptureKit system audio (macOS 13 or later) | yes |
| Linux | yes: monitor of the default output, PipeWire (pipewire-pulse) or PulseAudio | yes (ALSA, also through PipeWire and PulseAudio) |
| Web client (Pro) | — | yes, in browsers with WebCodecs' `AudioDecoder` for Opus (Chrome, Edge, Firefox on a computer, recent Safari) |

Session recordings have the sound too: a second track in the MP4 file ([31](31-session-recording.md)).

## How

- **Capture** (`client/audio/src/capture.rs`): `cpal` opens the default output device as an input, which on Windows is WASAPI loopback: the mix sent to the speakers, before the volume. The device's format (often 48 kHz float, sometimes 44.1 kHz or more channels) is converted to 48 kHz stereo (`rubato`). Windows's buffer is opened at 100 ms instead of its default of about 10 ms: on a busy computer the capture thread can be late, and with the small buffer Windows drops the sound meanwhile (an audible gap). It adds no delay, the data is read as soon as it is there. The gaps Windows still reports are counted and logged every 10 s; the player logs its losses, underruns and trims the same way. WASAPI delivers nothing while nothing plays: no packets then.
- **Capture on Linux** (`client/audio/src/pulse.rs`): the monitor of the default output (`@DEFAULT_MONITOR@`, what goes to the speakers) through `libpulse-simple`, asked for at 48 kHz stereo float, one 20 ms packet per read. PipeWire answers the same calls through pipewire-pulse (the default on current distributions), and so does PulseAudio. The library is loaded at run time: no build or package dependency, and without it the agent does not offer the sound. With PipeWire the monitor comes before the volume and the mute, like WASAPI: the person at the computer can turn the speakers down or mute them and the technician still hears (tried at 10% and muted, same level). With PulseAudio itself the monitor follows the volume. The monitor gives zeros when nothing plays.
- **Capture on macOS** (`capture.rs`, `open` for macOS): a ScreenCaptureKit stream of the main display with `capturesAudio`, 48 kHz stereo, its own sound left out (`excludesCurrentProcessAudio`). The stream needs a screen output: 2×2 pixels once a second, never read. It is the Screen Recording permission the screen capture already has: nothing more to allow. Like WASAPI it takes the sound before the volume and the mute.
- **Silence** is not sent: a 20 ms frame of only zeros is skipped (on Linux the monitor gives them all the time).
- **Codec** (`client/audio/src/opus.rs`): Opus at 48 kHz stereo, 20 ms packets, 96 kbit/s, with in-band redundancy (FEC) for one lost packet. The library is `unsafe-libopus`, libopus translated to Rust: no C compiler or cmake in the build.
- **Transport** (`client/audio/src/packet.rs`): each packet is one QUIC datagram, `0xA0, seq u32, Opus packet`: a lost one is not retransmitted (it would arrive too late), and it does not hold up anything else. The first byte tells it apart from the video's datagrams (`nuvira_transport::fec`, version 1). Over the WebSocket fallback the datagrams travel inside the tunnel like the rest.
- **Playback** (`client/audio/src/player.rs`): a decoding thread fills a buffer that the output device's callback empties. It starts playing once the target is there (60 ms at first), plays silence and waits to refill when it runs dry, and drops the oldest audio beyond the target plus 190 ms, so the delay stays short. The target adapts: a slow or overloaded computer sends its sound in bursts, and each drop is a tear, so each drop raises the target by 40 ms (up to 300 ms) and 30 s without one bring it down by 20 ms. On a normal computer it stays at 60 ms. A lost packet is rebuilt from the next one's redundancy; up to 5 in a row are made up by Opus's concealment, beyond that it is silence (the sound stopped).

## In recordings

The viewer writes the packets it receives into the recording as they are (no second encoding) as an Opus track next to the video (`client/viewer/src/mp4.rs`: `Opus` sample entry with `dOps`, 48 kHz stereo, fragments of 1 s of their own). The packets carry no time, and silence is not sent, so the recorder (`client/viewer/src/recorder.rs`) places each one by when it arrived: the arrival time is moved to the agent's clock with the difference measured on the video frames (arrival minus capture time, the lowest seen, let up slowly so drifting clocks are followed); packets that follow one another (`seq` + 1) stay exactly 20 ms apart unless that would put them more than 200 ms from their arrival. Slots without a packet (silence, losses, sound muted) get a silent Opus packet, so the track has no holes and players keep it in step with the picture. The sound is in the file only when the session has it: the viewer has an output device and the agent sends it; a muted session records silence.

## Rules and the person at the computer

Like the other permissions ([32](32-session-permissions.md)):

- **Rule "No sound of the computer"** (`Policy.no_audio`, field 13): from the server's rules (app → Administration, Pro console → Rules), or as a limit of a technician's account or of a member of a shared book (Pro, limit bit 12). The agent then does not offer the sound (`Hello.audio` false).
- **Switch "Sound"** in the strip of the app, for the person at the computer: off, the capture stops at once and nothing is sent; `Permissions.audio` (field 9) tells the viewer, which greys out its sound button and shows "Switched off by the person at the remote computer: Sound". On again, the sound comes back (unless the technician muted it).

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
- Linux, 2026-11-12, the Ubuntu 26.04 VM (PipeWire 1.6, Wayland, a dummy output) to the NUC through the NAS's server, same test tone played with `pw-play`: 9.00 s received, frequencies exact, no gaps, the silence around it not sent.
- Linux, 2026-11-12, a laptop (Celeron, built-in sound card) on an Ubuntu 26.10 live session (PipeWire, Wayland) and the Linux Mint 22.3 VM (PipeWire 1.0.5, X11), same tone: 451 and 452 packets, none lost, 9.00 s, no gaps. On the laptop the same with its output muted and at 10% volume.
- macOS, 2026-11-12, the rented Mac mini M2 Pro (macOS 27.2) to the NUC over the internet (direct, Paris to Italy), the tone played with `afplay`: 452 packets, none lost, 9.00 s, no gaps; the same with the Mac's output muted.
- Sound switch, 2026-11-12, PC-LABORATORIO with the app of this branch installed (service), the viewer on the NUC writing a WAV: three tones, the second with "Audio" switched off in the app's strip (pressed through UI Automation): the viewer got `audio: false` and "switched off at the remote computer: Audio", the agent stopped the capture, and only the first and third tones arrived (903 packets, 18.06 s, none lost); on again, the capture started at once.
- Listening, 2026-11-12, the Celeron laptop to the NUC, a YouTube video (load 6.9 on 4 cores, video at 10 to 18 fps): with the fixed 60 ms buffer 1 to 41 drops every 10 s, tears often; with the adaptive one the target settled at 140 ms after two drops in the first 30 s, then almost clean.
- Listening, 2026-11-12, PC-LABORATORIO (an old PC) to the NUC, a YouTube video, scrolling the screen meanwhile: good sound, video and sound together. With cpal's default buffer Windows dropped sound 5 to 10 times every 10 s (small tears); with the 100 ms buffer 0 to 2 times, rare tears on that PC only at full load. The viewer lost no packet and never ran dry.

- Recordings, 2026-11-13: unit test (3 s of video and a 440 Hz tone with 0.5 s of silence not sent, clocks 5 s apart): ffprobe reads `h264` 320x240 and `opus` 48 kHz stereo, both 3.0 s; decoded, the tone is at 440 Hz and the silence exactly from 1.0 to 1.5 s. A real session, PC-LABORATORIO's agent through the NAS's server to the NUC's viewer with `--record`, the three test tones played on PC-LABORATORIO: 1280x1024 video of 15.9 s and Opus track of 14.7 s, the three tones (9 s) from 5.5 s with the silence before them filled.
- Web client, 2026-11-13: headless Edge 157 on the NUC (the web client's test bench: page on localhost, WebSocket to the NAS's server) towards PC-LABORATORIO's agent, the three test tones: 451 buffers, 9.02 s played, the mean frequency of the three tones (1314 Hz); with the sound button muted, the agent stopped capturing and nothing arrived during a whole tone; unmuted, the next tone complete.
- macOS, 2026-11-13, a Mac mini M1 (macOS 27.0, release 0.25.0): the NUC's viewer recording the Mac playing the test tone: Opus track in the MP4, 451 packets, none lost. The Mac's viewer recording PC-LABORATORIO's three tones: the three tones complete in the Mac's MP4.

## To do

