# NuviraHub — Session recording

[← Index](README.md)

The technician can save a video of a session on their own computer. The person at the controlled computer is always told while it happens, and can refuse recordings. With a NuviraHub Pro server the recordings also reach the server, where the web console plays them, and a rule can record every session.

- **From the viewer.** The toolbar button with a round dot starts and stops a recording; while it runs the dot is red. It appears only when the agent allows recordings. `nuvira viewer --record` records from the start, `--record-dir DIR` chooses the folder.
- **From the app.** Settings → "Session recordings": "Record my sessions automatically" starts the viewer with `--record`; the folder can be changed ("Change folder") and opened. The default folder is `Videos\NuviraHub` (Movies/NuviraHub on macOS, `XDG_VIDEOS_DIR` or `~/Videos` on Linux). Choices in `app.json` (`record_sessions`, `record_dir`).
- **At the controlled computer.** While the session is recorded, the strip of the app at the top says "Recorded" with a blinking red dot, and the app comes to the front when a recording starts. The notice is repeated to an app opened during the session.
- **Not allowing it there.** App settings → "This computer" → "Allow session recordings" (on by default). The app saves the choice (`app.json`, `no_recording`) and sends it to its agent, or to the service's one (`AgentCommand::AllowRecording`); the agent keeps it in `permissions.json` like the other permissions. Not allowed: the agent does not announce recordings, refuses `SetRecording` with `@agent.recording_not_allowed`, and stops a recording in progress at once if the setting is unticked during it. A viewer started with `--record` then says "Not recorded: the person at the remote computer does not allow recordings".
- **Files.** One MP4 file per recording, named after the date, the time and the remote computer, e.g. `NuviraHub 2026-11-07 09.45.07 PC-LABORATORIO.mp4`. When the size of the remote picture changes (another screen, all the screens, a resolution change at the remote computer) the file is closed and the recording goes on in `… (2).mp4`, and so on. A recording started during the session drops its first instant if the full-size picture follows within 1.5 s (the keyframe asked for still has the window's size), instead of making a file of it. After a remote restart with reconnection the recording goes on in a new file. The viewer writes `@recorded<TAB>path` on its standard output for every file closed (read by the app).

## NuviraHub Pro: recordings on the server

With a Pro server whose licence has the recordings (entitlement `SESSION_RECORDING`, feature `pro.recording`):

- **Upload.** Every file the viewer closes is also sent to the server the session went through, by a process of its own (`nuvira viewer upload …`, hidden), so it goes on after the window is closed. The file stays on the technician's computer too. The process signs in to the server with the viewer's key, like the session did; the server takes the file only from the device that was the viewer of that session, accepted, at most 7 days before. It travels on a further stream of the QUIC signalling connection (`Upload`, `UploadReady`, `UploadResult` in `proto/nuvira/v1/rendezvous.proto`), encrypted and with the server's certificate checked like everything else, also through the WebSocket fallback; the server's HTTP port is not used. An interrupted upload goes on from the bytes the server already has; failures are tried again for about six hours (10 s, 30 s, 1 min … 1 h). What happens is in `upload.log` in the viewer's data folder.
- **Console.** "Recordings" tab: date, from, to, account (when the technician's app was signed in), file, size; "Play" opens a player in the page (the server answers Range requests, so seeking works), "Download", "Delete". Files arriving or interrupted show how much arrived. Files are kept `NUVIRA_RECORDING_DAYS` days (default 90) in `recordings/` in the server's data folder; files larger than `NUVIRA_RECORDING_MAX_GB` (default 8) are refused.
- **Rule "Record every session"** (console → Rules, app → Administration, `Policy.record_always`). Every session of the server's devices is recorded and sent to the server: the agent announces `Hello.recording_required`, the viewer starts recording at once and its button is locked ("Recorded: required by the server's rules"). The person at the PC is always told; in the app the setting "Allow session recordings" is ticked and locked, with "Required by your organisation's rules" (`AgentEvent::Rules`). A viewer that does not record (versions before this one, the web client) is refused with `@agent.recording_required`; if the recording does not start within 15 s, or the viewer stops it, the agent ends the session. Terminal-only sessions are not concerned.
- **Base server.** It takes no file: an upload gets `@server.pro_required`, and the viewer does not try again. The streams are served by modules through `ext.UploadDesk` (`server/internal/signal/uploads.go`).

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
- The web client of NuviraHub Pro does not record: with the rule "Record every session" it cannot connect.
- Viewers older than this version show the refusal of the rule as `@agent.recording_required` (they do not have the text).

## Tests

2026-11-07, PC-LABORATORIO (Windows 10 22H2, 1280x1024, Intel HD 2500, CPU rendering) with this build installed as the service, its app open in the user's session, and a viewer on the same PC towards its own ID through the NAS server, with `--record --duration 30`:

- The agent confirmed the recording 0.2 s after the connection; the app's strip showed "PC-LABORATORIO is controlling this computer · direct connection ● Recorded" (window capture of the app), and no longer after the session.
- One file of 8.0 MB, 22.3 s, 1280x1024 (about 2.9 Mbit/s for an office desktop). Opened in Microsoft Edge (Chromium's demuxer): duration and size right, seeking to 10 %, 50 % and 95 % showed the right frames. Windows Explorer (Media Foundation property handler) reads 22 s and 1280x1024. `@recorded` arrived on the standard output when the window closed.
- First try, before the full-size stream: the agent's first keyframe was 1280x1024 and the next ones 900x720 (window size), so the recording was split into a 0.5 s file and a 22.5 s one; both played in Edge.
- With "Allow session recordings" unticked in the app (`no_recording` in `app.json`) the viewer with `--record` connected, the agent announced no recordings and no file was written.
- Unit tests: Annex B splitting, sizes from the SPS of real OpenH264 streams (1920x1080, 1366x768, 330x202 with cropping), fragments, `mehd` and `mfra`, a new part at a resolution change, nothing written from a lost frame to the next keyframe.

Toolbar button, same day, same setup without `--record`: pressed through UI Automation (no mouse) after 8 s, the recording started (app strip "Recorded"), pressed again after 10 s, it stopped; one file of 10.1 s at 1280x1024 that plays in Edge. Before the fix of the first instant, the keyframe at the window's size made a 0.3 s file of its own.

NuviraHub Pro, same day, Pro test server on the Ubuntu VM (licence with `SESSION_RECORDING`), a test agent on PC-LABORATORIO registered there and a viewer on PC-LABORATORIO:

- With `--record`: 11 s recorded (4.0 MB); the upload process started when the file closed and the server logged "file received" 0.5 s later (117 Mbit/s on the LAN). In the console (Edge, Italian) the "Recordings" tab listed it; "Play" showed 1280x1024, 11.2 s, seeking to the middle worked; the download link answered 206 with `Content-Disposition: attachment`.
- Rule "Record every session" on (console API): a viewer without `--record` recorded from the start ("the server's rules want this session recorded") and the file reached the server. The viewer of release 0.16.0 was refused: "access denied: @agent.recording_required".
- Go tests of the module: refused for another device, bad names, sessions not accepted or older than 7 days, other kinds; an upload stopped after 1000 bytes goes on from there; sent again when complete, nothing is sent; delete removes the file; one upload at a time per file; nothing without the licence.

Not tested yet: unticking the setting during a recording, the locked setting in the app with the rule (the app was on another server), an interrupted upload in real life, a recording through the relay with video as datagrams, Linux and macOS viewers, playback in players other than Edge (Windows Media Player, VLC), a recording through the relay with video as datagrams, Linux and macOS viewers.
