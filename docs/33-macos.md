# NuviraHub — macOS

[← Index](README.md)

Since 0.15.0 the app runs on macOS 13 or later (one universal `.app` for Apple silicon and Intel, in a `.dmg` signed with a Developer ID and notarized). It controls other computers and a Mac can be controlled. Unattended access works as a launchd LaunchAgent (below). Not released yet. Automatic updates exist only for Windows for now.

## Package

`.github/workflows/client-macos.yml` (runner `macos-26`, for the ScreenCaptureKit SDK) builds both architectures, joins them with `lipo` and signs and notarizes them with `tools/package-macos.sh`. The output is `nuvira-macos.dmg`. Bundle identifier: `io.github.nuvira-hub.nuvira`.

## Controlled Mac

- **Screen**: ScreenCaptureKit (`client/capture/src/macos.rs`), BGRA frames with the cursor, at the display's pixel size (2× on Retina).
- **Mouse and keyboard**: Quartz events posted at the HID level (`client/input/src/macos.rs`).
- **Encoder**: H.264 with VideoToolbox (`client/codec/src/vt.rs`), the media engine of Apple Silicon or Quick Sync on Intel Macs. Low-latency rate control (one frame out for every frame in, Constrained Baseline) when the Mac has it, otherwise a real-time session in Main profile without B-frames. The BGRA frames go in as they are: the encoder converts them to YUV with BT.601, as the viewer expects. The output (AVCC) is turned into Annex B with SPS/PPS before every keyframe. With `--encoder auto` the agent measures it against OpenH264 at start and keeps the faster (hardware wins unless software is 25% faster); `--encoder hardware` refuses Apple's software encoder.
- **Privacy mode**: black screens in a child process left out of the capture, local input dropped by an event tap; see [24-privacy-mode.md](24-privacy-mode.md#how-it-works-macos).
- **Permissions**: macOS gives the screen and accepts input only once the person allows NuviraHub in System Settings → Privacy & Security → **Screen Recording** and **Accessibility**. The app asks for both at start. A copy of the program signed differently, or not signed, would need them again.

### Keyboard

Keys are sent **by position**: the viewer sends the W3C code of its physical key, and the Mac types the character of its own layout at that position. With an Italian PC keyboard and the "Italian – Pro" Mac layout, the letters with accents are where they are printed.

- **Modifiers.** Every key and mouse event carries the flags of the modifiers held (Shift, Control, Option, Command, Caps Lock). Modifier keys are posted as `FlagsChanged`, as a real keyboard does. Without the flags many apps typed "a" for Shift+A or "è" for Shift+è, and Cmd+click was a plain click.
- **ISO keyboards** (Italian, German, French…): the key left of 1 and the key left of Z have each other's virtual codes (`kVK_ISO_Section` and `kVK_ANSI_Grave`). The type comes from Carbon's `KBGetLayoutType(LMGetKbdType())`; `NUVIRA_MAC_KEYBOARD=iso|ansi` on the agent forces it. A Mac with no keyboard attached reports ANSI.
- **From a Mac viewer.** Towards Windows and Linux, Cmd and Ctrl are swapped, so Cmd+C copies. Towards a Mac they stay what they are (`Hello.os == "macos"`).
- **From a Windows or Linux viewer** to a Mac, Ctrl stays Ctrl, so copying is Win+C (Win is Cmd).

## Unattended access (LaunchAgent)

`nuvira service install` (or **Install** in the app's settings) writes `~/Library/LaunchAgents/io.github.nuvira-hub.nuvira.agent.plist` and loads it into the person's session (`launchctl bootstrap gui/UID`). It is the counterpart of the Linux user service (`client/service/src/launchd.rs`):

- launchd starts `nuvira agent --ipc --server …` at every login, in the desktop (Aqua) session, and starts it again if it ends (`KeepAlive`, 5 s apart).
- The program is the one **inside the app bundle**, so macOS sees the same signed app and the Screen Recording and Accessibility permissions hold for the agent too. Installing from the disk image (`/Volumes/…`) or from a translocated copy (`…/AppTranslocation/…`) is refused: the app must first be moved into Applications.
- Permanent password: a hash in `service.env` (mode 0600, in `~/Library/Application Support/nuvira`). It is handed to the agent as `NUVIRA_PASSWORD_HASH` in the definition, which is mode 0600 too.
- The app talks to the agent over the same Unix socket as on Linux.
- Log: `~/Library/Logs/NuviraHub/agent.log`.
- `AssociatedBundleIdentifiers` makes System Settings → General → Login Items list it under NuviraHub. Switched off there, it is shown as stopped, and "Start the service again" loads it again.
- **Remove**, in the app or `nuvira service uninstall`, unloads it (`bootout`) and deletes the definition and the password. The ID stays.
- `nuvira service start` restarts it (`kickstart -k`), or loads it if it is not loaded.

Limits: the Mac is reachable only while someone is logged in and the screen is not locked. After a restart it needs automatic login, which FileVault does not allow. The login screen would need a root LaunchDaemon (not done).

## Remote restart

See [27-remote-restart.md](27-remote-restart.md): the Apple event "restart" to loginwindow, no administrator needed.

## Tests of 2026-11-07 on a Mac mini M4

Scaleway Mac mini M4, macOS 27.2, SIP on, FileVault off, a 1920×1080 virtual display; driven over SSH (`cliclick`, re-signed ad hoc because macOS 27 kills the unsigned release binary; `screencapture`), viewer on the Fedora VM through NuviraHub's public server. The app from the disk image of the CI (`spctl`: "Notarized Developer ID").

- **Permissions**: at its first start the app opened Accessibility with NuviraHub in the list. Screen Recording: macOS 27 adds NuviraHub to the list without any window, so the installation now opens that page itself, moves the app out of the way (apps can no longer bring another app in front) and says what to do (`@install.screen_permission_mac`); after "Quit & Reopen" the installation goes through.
- **Unattended access** from the app's settings: LaunchAgent running from the bundle, "App Background Activity" notification of macOS, same ID; the app switches to the service's agent at once (before the printer's administrator prompt, see below). Agent killed: launchd started it again within 1 s.
- **Remote restart** (`--restart` from the viewer): back online after about 50 s, automatic login, same ID, screen captured at once.
- **Session** with the permanent password: ScreenCaptureKit 1920×1080, VideoToolbox.
- **Printer**: the administrator prompt (macOS 27 adds "Apple could not verify this script is free of malware" and ignores the text `with prompt` gives it, so the app's message explains it while the prompt is open); with SIP on the backend goes into `/usr/libexec/cups/backend` (root:wheel 0700). A text and a PDF printed with `lp` reached the viewer as PDF with their titles. "Don't Allow": unattended access installed, the app says the printer was not.
- **Privacy mode**: a black window of the child process at the shielding level, full screen, alpha 1, sharing none (so no capture shows it, `screencapture` and VNC included: checked in the window list); keys and clicks of the Mac's own input did not reach TextEdit during it, did after.
- **VideoToolbox**: with the low-latency rate control the session does not report `UsingHardwareAcceleratedVideoEncoder` (now logged as unknown), without it hardware=true, same speed: about 4.5 ms per frame at 720p and 8 ms at 1080p in the bench (peaks 35–45 ms), where OpenH264 takes 1–2 ms on that nearly still image. The 15–16 ms per frame of a session at 720p came from the quiet screen: with 100 ms between frames (`NUVIRA_BENCH_PACE_MS=100`) VideoToolbox takes 8.5 ms and OpenH264 6.4 ms (instead of 1): the Mac lowers its clocks; the CPU scaling from 1080p takes 1.3 ms; `MaxFrameDelayCount` 0 and "speed first" changed nothing.
- **Automatic update** (channel `macos`, test server on the Mac, version 0.16.0.1): bundle swapped 2 minutes after start, the agent ended and launchd started the new one, same ID; Screen Recording and Accessibility still granted (capture and privacy mode in the next session).
- On a CI runner (no screen, SIP off): the same printer checks with a listener on the socket, and the swap refusing a changed and an ad-hoc signed app (`.github/workflows/macos-live-checks.yml`).

- **Terminal-only session**: login zsh as the user, commands and output both ways.
- **File manager** (agent): 3 MB from the Mac and 2 MB to its Desktop, same SHA-256. macOS asked at the Mac "NuviraHub would like to access files in your Desktop folder": Desktop, Documents and Downloads have permissions of their own, which nobody can give in an unattended session: the installation now opens the three folders, so macOS asks while the person is at the Mac (the permission is the app's, the agent is the same app); afterwards a file went to the Desktop in a session without any question.
- **Clipboard** both ways (text, Fedora's Wayland clipboard).
- **Recording**: the strip shows "Recorded"; the MP4 was saved by the viewer.
- **Session permissions**: "Keyboard and mouse" switched off in the strip reached the viewer at once (`switched_off: ["input"]`), the remote mouse stopped; on again afterwards. **Chat** from the Mac reached the viewer.
- **Keyboard** from the Mint viewer (real key events through XTest, Italian pc105 keyboard) into TextEdit: with the Mac on "US" by position "Ab1[;'\§±" (a Mac without a keyboard reports ANSI: no swap); with "Italian – Pro" and `NUVIRA_MAC_KEYBOARD=iso` in the LaunchAgent "Ab1èòàù<>". (`launchctl setenv` does not reach the job: the variable goes into the property list.)
- **The Mac as the viewer** towards Mint: window, toolbar, Dock icon; recording into `~/Movies/NuviraHub` (H.264 1280×800, read by Spotlight); a document printed on Mint opened the "Print" window with the Mac's printers (its own NuviraHub left out) and went into the chosen CUPS queue; terminal window; file manager window: its own side was empty (Windows folders only), fixed, then a file sent from the Mac's Desktop to Mint with the same SHA-256.
- `nuvira viewer --server ""` is now NuviraHub's public server, as for the agent.

- With a **NuviraHub Pro** test server on the Mac itself (built for macOS from nuvira-pro, the test licence): **TCP tunnel** from the Fedora viewer to `127.0.0.1:8000` of the Mac (500 kB, same SHA-256), `8.8.8.8:53` refused by the agent; **RMM inventory** of the Mac in the console (macOS 27.2, Apple M4, 10 cores, 16 GB, disks, uptime; now with the build of macOS as the version, and the APFS volumes of /System/Volumes no longer counted twice); **address book sign-in with 2FA** in the Mac's app (password, then the TOTP code). Single sign-on not tried (no identity provider).
- **The Mac as the viewer towards Windows** (a test agent on PC-LABORATORIO): picture at 1280×1024, the "Ctrl+Alt+Del" button in the toolbar (not pressed).
- **IPv6**: an agent with a synthetic picture on the OVH VPS (Canada, in a Debian 12 container with the host's network) registered on the Mac's server by its IPv6 address; the Mac's viewer went **direct over IPv6** (`path=Direct remote=[2607:5300:…]`, agent "viewer connected directly ([2001:bc8:…])"): 29.9 fps, latency p50 55 ms, p95 60 ms; the viewer draws with Metal.

## Not tested yet

Single sign-on; Mac to Mac (Cmd kept); several displays, a Retina/5K screen.
