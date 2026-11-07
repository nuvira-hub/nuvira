# NuviraHub — Session recording

[← Index](README.md)

The technician can save a video of a session on their own computer. The person at the controlled computer is always told while it happens, and can refuse recordings.

- **From the viewer.** The toolbar button with a round dot starts and stops a recording; while it runs the dot is red. It appears only when the agent allows recordings. `nuvira viewer --record` records from the start, `--record-dir DIR` chooses the folder.
- **From the app.** Settings → "Session recordings": "Record my sessions automatically" starts the viewer with `--record`; the folder can be changed ("Change folder") and opened. The default folder is `Videos\NuviraHub` (Movies/NuviraHub on macOS, `XDG_VIDEOS_DIR` or `~/Videos` on Linux). Choices in `app.json` (`record_sessions`, `record_dir`).
- **At the controlled computer.** While the session is recorded, the strip of the app at the top says "Recorded" with a blinking red dot, and the app comes to the front when a recording starts. The notice is repeated to an app opened during the session.
- **Not allowing it there.** App settings → "This computer" → "Allow session recordings" (on by default). The app saves the choice (`app.json`, `no_recording`) and sends it to its agent, or to the service's one (`AgentCommand::AllowRecording`); the agent keeps it in `permissions.json` like the other permissions. Not allowed: the agent does not announce recordings, refuses `SetRecording` with `@agent.recording_not_allowed`, and stops a recording in progress at once if the setting is unticked during it. A viewer started with `--record` then says "Not recorded: the person at the remote computer does not allow recordings".
- **Files.** One MP4 file per recording, named after the date, the time and the remote computer, e.g. `NuviraHub 2026-11-07 09.45.07 PC-LABORATORIO.mp4`. When the size of the remote picture changes (another screen, all the screens, a resolution change at the remote computer) the file is closed and the recording goes on in `… (2).mp4`, and so on. After a remote restart with reconnection the recording goes on in a new file. The viewer writes `@recorded<TAB>path` on its standard output for every file closed (read by the app).

## How it works

- **No new encoding.** The agent's H.264 frames (Annex B) go into the file as they arrive: no extra CPU, and the quality of the video is the quality of the session. The decoding thread of the viewer writes them (`client/viewer/src/recorder.rs`), after the agent confirmed the recording, starting at a keyframe it asks for. Frames lost on the way (video as datagrams) break the stream until the next keyframe: they are skipped, the picture stays still.
- **Full screen while recording.** The agent normally reduces the picture to the size of the viewer's window (`ViewSize`); while recording the viewer asks for the full size, as in the 1:1 mode, and reduces it itself on screen. The text is readable in the video, and resizing the window does not cut the file. The cost is the bandwidth of the full-size stream.
- **Fragmented MP4** (`client/viewer/src/mp4.rs`, written by us, no library): `ftyp` + `moov` with an `avc1` track (90 kHz, `avcC` from the SPS and PPS of the first keyframe), then a `moof` + `mdat` fragment about every second, each starting at a keyframe when there is one. A crash, a full disk or a lost connection loses at most the last second. At the end the duration goes into `mehd` and an `mfra` index of the keyframes is appended for seeking. The times are the agent's capture times. Parameter sets equal to the header's are left out of the samples; different ones (same size) stay in band.
- **Protocol** (`proto/nuvira/v1/session.proto`). `Hello.recording`: from the viewer, it can record; from the agent, the person at the PC allows recordings. The viewer sends `SetRecording { on }`, the agent tells its app (`AgentEvent::Recording`) and answers `RecordingState { on, error }`; it also sends `RecordingState { on: false, error }` by itself when the setting is unticked during the recording. The viewer writes only while the last state is `on`.

## Limits

- Only the video: there is no audio in NuviraHub sessions. Chat, file transfers and terminals are not in the video (the session log of NuviraHub Pro has the transfers).
- The notice relies on the viewer being honest: a modified viewer, or any screen recorder on the technician's computer, can record without telling. The setting is a statement of the person's wishes that NuviraHub's own viewer respects, not a protection.
- Without the app running at the controlled computer (service, nobody signed in) nobody sees the notice.
- Terminal-only sessions are not recorded.
- The web client of NuviraHub Pro does not record.

## Tests

2026-11-07, PC-LABORATORIO (Windows 10 22H2, 1280x1024, Intel HD 2500, CPU rendering) with this build installed as the service, its app open in the user's session, and a viewer on the same PC towards its own ID through the NAS server, with `--record --duration 30`:

- The agent confirmed the recording 0.2 s after the connection; the app's strip showed "PC-LABORATORIO is controlling this computer · direct connection ● Recorded" (window capture of the app), and no longer after the session.
- One file of 8.0 MB, 22.3 s, 1280x1024 (about 2.9 Mbit/s for an office desktop). Opened in Microsoft Edge (Chromium's demuxer): duration and size right, seeking to 10 %, 50 % and 95 % showed the right frames. Windows Explorer (Media Foundation property handler) reads 22 s and 1280x1024. `@recorded` arrived on the standard output when the window closed.
- First try, before the full-size stream: the agent's first keyframe was 1280x1024 and the next ones 900x720 (window size), so the recording was split into a 0.5 s file and a 22.5 s one; both played in Edge.
- With "Allow session recordings" unticked in the app (`no_recording` in `app.json`) the viewer with `--record` connected, the agent announced no recordings and no file was written.
- Unit tests: Annex B splitting, sizes from the SPS of real OpenH264 streams (1920x1080, 1366x768, 330x202 with cropping), fragments, `mehd` and `mfra`, a new part at a resolution change, nothing written from a lost frame to the next keyframe.

Not tested yet: the toolbar button, unticking the setting during a recording, playback in players other than Edge (Windows Media Player, VLC), a recording through the relay with video as datagrams, Linux and macOS viewers.
