# NuviraHub — The technician's microphone

[← Index](README.md)

The technician can talk to the person at the controlled computer: the viewer's
microphone comes out of that computer's speakers. It is the way back of the
remote sound ([38-remote-audio.md](38-remote-audio.md)) and uses the same pieces.

- **Viewer**: the default input device through cpal (WASAPI on Windows, ALSA —
  PipeWire's or PulseAudio's plugin — on Linux, Core Audio on macOS), converted
  to 48 kHz stereo, Opus for speech (`OPUS_APPLICATION_VOIP`, 32 kbit/s, 20 ms
  packets, in-band FEC), each packet a QUIC datagram in the remote sound's format
  (`nuvira_audio::packet`). `nuvira_audio::microphone`.
- **Agent**: reads the datagrams and plays them with the remote sound's player
  (adaptive buffer, Opus concealment of lost packets) on its default output.
- **Off at the start of every session**: the microphone button in the session
  bar turns it on and off (`SetMicrophone`); the agent answers
  `MicrophoneState { on, error }`.
- **The person at the computer**: while it plays, the app's strip says "The
  technician is talking: mute"; one click mutes it for the session (the agent
  drops the packets) and the technician's bar shows "the person at the remote
  computer muted your microphone"; another click listens again.
- **NuviraHub Pro**: the rule "No technician's microphone" (`Policy.no_microphone`,
  also as a limit of an account or a member) — the agent does not offer it.
- `Hello.microphone`: from the viewer, it has a microphone; from the agent, it
  can play it (a server session, a sound output, allowed by the rules). Not for
  guests of a shared session, nor terminal-only sessions.
- macOS: the app's `Info.plist` has `NSMicrophoneUsageDescription` and its
  signature the entitlement `com.apple.security.device.audio-input` (the hardened
  runtime refuses the microphone without it); macOS asks the technician once.
- Tests: `--microphone` (viewer: on as soon as connected), `NUVIRA_MIC_DUMP=FILE`
  (agent: written to a WAV file instead of played).

While muted by the person at the computer the viewer keeps capturing and sending
(the agent drops the packets): unmuting needs nothing from the technician.

## Tests of 2026-11-15

| Viewer (microphone) | Agent (playback) | Result |
|---|---|---|
| PC-LABORATORIO, Windows, the LifeCam HD-3000's microphone (mono 48 kHz) | Mint, Cinnamon (WAV) | 9 s of the room's sound (RMS 0.0013, not silence) |
| Mint, the default source = monitor of the output playing a 440 Hz tone | PC-LABORATORIO, Windows (WAV) | 8.5 s at 440.0 Hz, peak 0.253 for 0.244 sent |
| PC-LABORATORIO | Ubuntu GNOME, Kubuntu KDE (WAV) | played (1.37 and 1.39 MB of WAV for ~7 s each) |
| PC-LABORATORIO, the bar's button through UI Automation | Mint | on, then off: the agent followed both |
| PC-LABORATORIO | Mint with the app of this build (IPC agent) | the strip's button muted it (the WAV stopped growing, the viewer showed the message) and unmuted it (growing again) |
| PC-LABORATORIO, `--microphone` | Mint registered on the Pro test server with "No technician's microphone" | not offered: no capture, nothing played |

Not tried yet: a Mac as viewer (microphone permission, entitlement) or as
controlled computer; real speakers (playback was always written to a WAV file
except in the remote sound's earlier tests of the same player); a Linux viewer
with a real microphone (only a monitor source); the web client (it has no
microphone yet); the app's rules editor field (the console's checkbox and the
limit of sessions without an account were tried in the console, through Firefox).
