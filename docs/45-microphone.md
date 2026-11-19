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
| Mint (440 Hz tone) | PC-LABORATORIO's own speakers (Realtek), no WAV | 330 packets played, none lost, no underrun |

On the Mac mini M1 (Scaleway, macOS 27.2.1, 2026-11-16; app built there from the branch, signed with a fixed test identity):
- the Mac as controlled computer: PC-LABORATORIO's viewer with `--microphone`
  (its LifeCam): "technician's microphone played" by the Mac's agent, its app's
  strip showed "The technician is talking: mute";
- the Mac as viewer: this Mac mini has no input device at all. cpal still answered a
  default input device (Core Audio's "unknown object"), so the button was offered
  and turning it on failed with an unreadable "error unknown to the Core Audio
  … 560947818" ('!obj') written over the session bar's buttons. Fixed: a device
  that cannot give its input format counts as none (the microphone is not offered),
  the error is a readable sentence, and a long notice is cut in the bar (whole on
  hover).

The app's rules editor field, 2026-11-16: PC-LABORATORIO's app (administrator of
the Pro test server), Administration, "No technician's microphone" ticked and
saved: the server's rule `no_microphone: true`; PC-LABORATORIO's viewer with
`--microphone` towards the Mac: no microphone started, nothing asked at the Mac;
the rule off again (control): "microphone at the remote computer on=true".

The Mac viewer with a microphone, 2026-11-16: BlackHole 2ch 0.7.1 installed on the
Mac mini (checksum as Homebrew's, signed and notarized) as its input device, `say -a
"BlackHole 2ch"` talking into it; the Mac's viewer towards PC-LABORATORIO's service
with `--microphone`: macOS asked "Allow "NuviraHub" to access your microphone?"
with our text (`NSMicrophoneUsageDescription`, the audio-input entitlement of the
signed app) → Allow → "microphone started channels=2 rate=48000", LAB's service:
"technician's microphone played".

Not tried yet: real speakers on Linux and a Linux viewer with a real microphone (the VMs
have none: a PC with a live Linux is needed).

## The web client (2026-11-16)

The page offers the microphone when the browser can capture and encode it
(`getUserMedia`, WebCodecs `AudioEncoder` with Opus, a secure context) and the agent
can play it: `Hello.microphone` from the page, the button 🎙 in the bar ("Talk: your
microphone…", on: "Microphone on: click to turn it off"). The capture goes through
Web Audio (a ScriptProcessor: no worklet file under the page's CSP), mono duplicated
to stereo, encoded at 48 kHz, 32 kbit/s, 20 ms frames, and each packet goes as a
datagram in the format of the desktop viewer (`0xA0, seq, packet`). The agent's
`MicrophoneState` comes back to the page: off with a reason (muted at the computer, a
rule) turns the button off and shows the reason. Off at every connection. If the
session ends while the browser is still asking for the microphone (its permission
prompt open), the capture that the answer starts is stopped at once and nothing is
shown; the packets of a capture go only to the session it was started for, never to
a later session of the page (review lot 10, WEB-4; before, the capture kept running
until the page was reloaded and its packets went to the next computer). Tried
headless on the NUC with fake devices in Chromium 156 and Firefox 157; not tried on
Safari.

| Browser | Microphone | Towards | Result |
|---|---|---|---|
| Chrome 157 (PC-LABORATORIO) | LifeCam, Chrome's prompt "Consenti questa volta" | the Mac's agent | played, 1385 packets in 27.7 s, 0 lost |
| Chrome 157, fake device playing a 440 Hz tone | `--use-file-for-fake-audio-capture` | a test agent on the Mac with `NUVIRA_MIC_DUMP` | the recording at the agent: 440 Hz five orders of magnitude above 220-1000 Hz |
| Safari 27 (Mac mini) | BlackHole with `say`, Safari's "Allow localhost to use your microphone?" | PC-LABORATORIO's service | played, 1528 packets in 30.7 s, 0 lost |
| Safari, Pro rule "No technician's microphone" | | PC-LABORATORIO's service | in a session started before the rule the microphone still played (rules hold from the next session, as every rule); a new session: no button |

Not tried: Firefox (it has AudioEncoder; the Pro test server has no HTTPS name for
it to trust, and plain HTTP is not a secure context there).
