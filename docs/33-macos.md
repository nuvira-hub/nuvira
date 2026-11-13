# NuviraHub — macOS

[← Index](README.md)

Since 0.15.0 the app runs on macOS 13 or later (one universal `.app` for Apple silicon and Intel, in a `.dmg` signed with a Developer ID and notarized). It controls other computers and a Mac can be controlled. Since 0.17.0: unattended access as a launchd LaunchAgent, also at the sign-in screen, remote restart, privacy mode, VideoToolbox, the NuviraHub printer and automatic updates ([19-updates.md](19-updates.md)).

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
- **ISO keyboards** (Italian, German, French…): the key left of 1 and the key left of Z have each other's virtual codes (`kVK_ISO_Section` and `kVK_ANSI_Grave`). The type comes from Carbon's `KBGetLayoutType(LMGetKbdType())`; `NUVIRA_MAC_KEYBOARD=iso|ansi` on the agent forces it; the `Keyboard` key of the Mac's managed preferences wins over it, also without the installation for the whole Mac ([35-macos-deployment.md](35-macos-deployment.md)). A Mac with no keyboard attached reports ANSI.
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

Limits: see the sign-in screen and the locked screen below. For organisations it can be installed for the whole Mac instead, with the Mac's own ID: `nuvira-macos.pkg` and a configuration profile ([35-macos-deployment.md](35-macos-deployment.md)). With FileVault on, the disk is unlocked before macOS starts: nothing of NuviraHub runs there.

## Sign-in screen

After a restart without automatic login, or after logging out, the Mac shows its sign-in screen; NuviraHub is reachable there too, with the same ID and the permanent password (`client/service/src/signin_mac.rs`).

- **Installed with the printer.** The app's Install runs `nuvira service system` after the single administrator prompt (`client/service/src/system.rs`): the NuviraHub printer and `/Library/LaunchAgents/io.github.nuvira-hub.nuvira.signin.plist`, a LaunchAgent limited to the `LoginWindow` session. launchd runs it as root while the sign-in screen is shown. It holds no secret: device key, server fingerprint and the password's hash are copied into `/Library/Application Support/NuviraHub` (root only, 0600; the key protected with the System keychain, [36](36-device-key.md)). Remove takes them away with the printer (one prompt). Changing the server in the app's settings installs the system part again with the new server (one administrator prompt), as on Linux ([20-linux.md](20-linux.md)): otherwise the sign-in screen would stay on the old server.
- **`nuvira service signin-run`** starts `nuvira agent --signin-screen`: only the permanent password is accepted (nobody could confirm a temporary one), no automatic updates (the person's agent does them). At start-up the network may not be there yet: the agent is started again every 5 s.
- **Same program, same permissions.** It is the program inside the app's bundle: Screen Recording and Accessibility hold for root too (`AXIsProcessTrusted` true, uid 0). macOS asks once more at the sign-in screen ("nuvira is requesting to bypass the system private window picker…"): the technician answers it with the mouse.
- **Pre-login app.** WindowServer delivers synthetic input at the sign-in screen only to programs whose binary has a `__CGPreLoginApp` section (Apple's CGPreLoginApp sample): `client/app/build.rs` adds it (`-sectcreate __CGPreLoginApp __cgpreloginapp /dev/null`). Without it the screen was captured but every click and key was dropped silently, with every event source and tap.
- **Input events without an event source.** A `HIDSystemState` source made for every event hung there (SkyLight's `CGSEventSourceForID` waiting on a lock): events are now made with a `NULL` source.
- **One agent at a time.** The server keeps the last connection with an ID: launchd ends the sign-in screen's job when someone signs in (and `signin-run` ends its agent as soon as SystemConfiguration's console user is a person); the person's LaunchAgent then registers with the same ID. The viewer connects again by itself.

## Locked screen

**Picture.** ScreenCaptureKit shows the lock screen of a signed-in person like any other screen, as long as the display is on. What hid it until 0.21: macOS turns the display off a few seconds after the lock (about 5 s on the Mac mini, also with `pmset displaysleep 0`); then ScreenCaptureKit lists no display and stops a running stream (error -3815, "Failed to find any displays or windows to capture"), and `CGDisplayCreateImage` gives black. The hint came from [LanternOps/breeze #10360 and #10874](https://github.com/LanternOps/breeze/issues/10874), where the same black screen turned out to be the display going off.

So while a capture is open (`client/capture/src/macos.rs`, `DisplayAwake`):

- a `PreventUserIdleDisplaySleep` power assertion is held (as `caffeinate -d`), named "NuviraHub remote session" in `pmset -g assertions`, released when the session ends;
- while the screen is locked, `IOPMAssertionDeclareUserActivity` every 10 s and at the lock: it also turns on a display that is already off (in 0.5 s), which the assertion alone does not;
- the stream stays open across lock and unlock; if ScreenCaptureKit stops it all the same (delegate `didStopWithError`), or no display is found at the start (display off, a closed laptop lid), the dark placeholder is sent once a second and the stream is opened again.

Tested 2026-11-09 on the Mac mini (macOS 27.2) from the viewer on PC-LABORATORIO: connecting to a Mac locked for minutes, with the display off: display on, stream open 0.6 s later, the lock screen seen at 51 fps (clock, user picture, password field, "Your screen is being observed"); a 150 s session at 51 fps, the display off again as soon as the session ended. Before the change the same session saw only the placeholder. With the virtual keyboard: a session open on the desktop, the Mac locked meanwhile: the lock screen at once, same stream; "abc" typed from the viewer showed three dots in the password field; Backspaces, the password and Enter unlocked it and the desktop came back in the same session.

The lock screen takes no synthetic input: `CGEventPost` with every source (none, HID, combined, private) and tap (HID, session, to loginwindow's pid), the old `CGPostKeyboardEvent`, as the person and as root, with and without the pre-login section; switching to the login window (`SACSwitchToLoginWindow`) was refused as well. A **virtual HID keyboard** works (below).

### Typing the password: a virtual HID keyboard

The lock screen takes the input of a HID device. The DriverKit driver of [Karabiner-DriverKit-VirtualHIDDevice](https://github.com/pqrs-org/Karabiner-DriverKit-VirtualHIDDevice) (public domain, signed and notarized by its author; 8.7.0 in `tools/vendor`, copied into the app's Resources with its SHA-256 checked) provides one (`client/service/src/hid_mac.rs`, `client/input/src/lockhid_mac.rs`):

- **Installed with the system part** (`nuvira service system`, the administrator prompt): its package, unless a Karabiner is already on the Mac, the keyboard type of the virtual device (ANSI, in `com.apple.keyboardtype`: otherwise macOS opens the Keyboard Setup Assistant at its first key), and our LaunchDaemon `io.github.nuvira-hub.nuvira.hid` (`nuvira service hid-run`, root). Then the app asks macOS to load the driver extension and, the first time, opens System Settings → General → Login Items & Extensions → Driver Extensions, where the person turns it on (`@install.hid_allow_mac`). Removed with unattended access: the app first deactivates the driver extension as the person (root cannot; macOS finishes at the next restart), then the system part removes our daemon and Karabiner's files, unless Karabiner-Elements is on the Mac and uses the same driver.
- **`hid-run`** listens on `/var/run/nuvira-hid.sock` and accepts only the program of this same bundle (the peer's path, `LOCAL_PEERPID` + `proc_pidpath`). It keeps Karabiner's daemon running and talks to it over its root-only socket in Rust (pqrs' unix_domain_stream frames: size u32 BE, type, request id u64 BE; requests carry the client protocol version 7; the daemon's status requests are answered, a heartbeat every 5 s). Two lessons from the Mac mini: a connection opened and closed at once makes that daemon's server close and bind again (so it is never probed by connecting), and unanswered status requests make it drop the client after 15 s.
- **The agent**, while `CGSSessionScreenIsLocked`, sends HID keyboard reports (modifiers + up to 32 usages) instead of Quartz events; it connects as soon as the screen locks, so that the keyboard is open before the first key, and `hid-run` spaces the reports 12 ms (a burst lost keys).
- `ScreenState.input` is true when the service is there: the viewer says to type the password and press Enter, and the technician sees the lock screen while typing.

Tested 2026-11-08 on the Mac mini (macOS 27.2): locked, the viewer on PC-LABORATORIO typed the password with Enter, the Mac unlocked 2 s later and the desktop came back in the same session.

So the session no longer ends when the Mac is locked: the technician sees the lock screen; the agent tells the viewer (`ScreenState`, `input` true when the virtual keyboard is installed), which shows a notice. Without the virtual keyboard (driver extension not allowed) someone must unlock it at the Mac, or the technician restarts it (toolbar) and signs in at the sign-in screen.

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
- **Frame pace (2026-11-09, 0.21)**: with a full-screen animation (`nuvira-viewer --example animate`, changes at every frame) sessions at 1080p stayed at 47 fps with either encoder (58 captured, about 11 a second skipped), while encoding took 4.7 ms. The cause was not the encoder: macOS gives an ordinary thread's timers a leeway of about half the time asked (a 12 ms sleep lasted 17, 8 ms lasted 11.3, a higher QoS changed nothing), so the pause between frames was 17.8 ms instead of 12. The encoding thread now has the time-constraint policy (`THREAD_TIME_CONSTRAINT_POLICY`, Apple's for audio and video), with which the sleeps are exact. Mac mini → NUC viewer through the NAS server, same agent started from SSH:

  | | fps | latency p50 / p95 | encoding |
  |---|---|---|---|
  | before, VideoToolbox | 47.3 (54 skipped every 5 s) | 36 / 48 ms | 4.8 ms |
  | after, VideoToolbox | 57.8 (none skipped) | 26 / 34 ms | 4.7 ms |
  | after, OpenH264 | 57.9 (none skipped) | 21.6 / 31 ms | 4.4 ms |

  On that simple picture both encoders take the same time; VideoToolbox's stream takes the Windows viewer twice as long to decode (7 ms against 3.5), hence the higher latency. A Retina or 5K display was not tried (the Mac mini has a 1920×1080 virtual one).

  With the signed package of the CI (run 42593247796) installed over the whole-Mac installation, the agent started by its LaunchAgent: 57.8 fps, none skipped, p50 32 ms; but VideoToolbox took 10 ms a frame there instead of 4.7 (the same signed program started with `launchctl asuser` from SSH: 4.4 ms, and its start-up test chose OpenH264). Not explained yet: with the synthetic picture (`--synthetic 1920x1080@60`) a LaunchAgent and SSH encode alike (13.8 ms), and a user-interactive QoS on the encoding thread changed nothing; plain CPU work runs at the same speed in the three contexts. Still within a 60 fps frame; to look at on a Mac.
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

## Tests of 2026-11-08 (sign-in screen)

On the Mac mini M4 (macOS 27.2), viewer on PC-LABORATORIO (Windows) through the public server, with `--type-keys` and `--click-at` (hidden test options of the viewer: text typed at a person's pace, a click):

- Installed with `sudo nuvira service system --user-data … --server ""` (what the app runs after the prompt); restart with automatic login switched off: the sign-in screen's agent registered with the Mac's own ID (470 150 788) about 8 s after the first attempt, which failed because DNS was not ready yet.
- Sign-in screen seen with the permanent password; the macOS prompt for Screen Recording appeared on it once.
- Before the pre-login section: no click or key reached it. With it: click in the password field, "abc" typed (three dots), then the Mac's password and Enter: signed in; launchd ended the sign-in screen's agent, the person's LaunchAgent registered with the same ID.
- Locked screen: the session stays open with the placeholder and `locked=true` in the viewer; typing the password does not reach the lock screen (watched through Screen Sharing: the field stayed empty with every method above).

## Tests of 2026-11-10 on a Mac mini M2 Pro

macOS 27.2.1, a 1080p virtual screen, Scaleway; driven over SSH and over Screen Sharing (VNC with Apple's authentication, so the clicks count as a person's: the privacy permissions were given that way, nobody needed at the Mac). Builds: the 0.22.0 release, and the branch `mac-keychain-no-window` built on the Mac.

- **Keychain.** With the login keychain locked, 0.21.1 creating a key showed "NuviraHub wants to use the login keychain" and waited (20 s and on, no key). The branch: a new key never shows a window and goes to the file at once (`NVK1F`, 2 s, from SSH as on a CI runner and in the person's session); with the keychain unlocked it goes to the keychain (`NVK1K`, same ID at the next start). An existing key keeps the window: with the keychain locked the agent waits, the key untouched; Cancel keeps it (before: errSecUserCanceled set the key aside and the Mac got a new ID), the password then lets the agent start with the same ID. A keychain item made by another build answers errSecAuthFailed with the window disabled: counted as "not now", never "unreadable" (the first try of the branch set the key aside and changed the ID).
- **Remote resolution** ([37-remote-resolution.md](37-remote-resolution.md)) did not work on a real Mac before three fixes (same branch): the mode list damaged by core-graphics 0.25 on its second read (only the current mode left: "no mode 1280x720"), modes valid but not safe for the desktop taken (kCGErrorFailure 1001), no Retina modes in the default list (a Mac in a Retina mode found none). After them: 1920x1080 to 1280x720 for the Mint viewer's 1280x720 area, back at the end of the session; `kill -9` of the agent: macOS put 1920x1080 back by itself; from 1280x720 Retina (2560x1440 pixels) a 1920x1080 area gave 960x540 Retina (1920x1080 pixels), put back to 1280x720 Retina.
- **Automatic update 0.21.1 to 0.22.0** (person's own installation, LaunchAgent, the NAS server with the 0.22.0 updates): 2 minutes after the start "version 0.22.0 available", downloaded, checked, installed, started again 15 s later; same ID (838 866 132), same signature (team 2T2G3ULLTF); Screen Recording kept (a session after the update had the picture).
- **Sign-in screen from the person's own installation** (Install in the app, the administrator prompt, the handed-over copy of the key): the sign-in screen's agent registered with the Mac's ID; from the Mint viewer the password typed with `--type-keys` signed m1 in, and the person's agent took the same ID. Over the sign-in window macOS asked once "nuvira is requesting to bypass the system private window picker" (as for every new program and person, 2026-11-08). The Karabiner driver needed its switch in Login Items & Extensions, Driver Extensions (the page the app opens) and the password.
- **A server change in the app moves the sign-in screen too** (Save: administrator prompt, both agents on the new server, with the ID they had there).
- **Bug found:** with unattended access installed with another server than the app's own (here by `service install --server`), Remove and Install in the app moved the Mac to the app's server (the public one) with a new ID, while the field showed the service's. Fixed on the branch: the app keeps the server the service reports; tried (an empty server in `app.json` became `alexbn.ddns.net` as soon as the app linked to the service). Note for tests: the service's agent serves one app at a time, a second copy of the app waits.
- **Clipboard images** both ways with the Mint viewer: 210x110 copied on the Mac arrived on Mint, 333x177 copied on Mint arrived on the Mac (PNG).
- **Session switches** in the strip of the app: clipboard, files, terminal, tunnels, printing and screen resolution off one by one and on again: the viewer got each change (`switched_off`).
- **Mac as viewer** (0.22.0) towards Kubuntu through the owner's Pro server: the Tunnels window (address, port, Add; Open in the browser opened Safari; Remove closed the port). A router that sends HTTP to HTTPS on the same port does not open through a tunnel to port 80 (the browser goes to https://localhost:PORT): add one to 443. Recording towards a still screen: 21.8 s of video for 21.7 s recorded, the still end kept.
- **VideoToolbox**: the agent's own measurement at its start, "hardware from RAM", is 9.2 ms a frame for 1080p in every context (the LaunchAgent, which already has `ProcessType` Interactive; the person's session; plain SSH), software 7.3 to 8.7 ms: launchd does not slow it down on this M2 Pro (the 4.7 ms of 2026-11-09 came from another Mac and another measure). Within the 16.7 ms of 60 fps; left as it is.
- **Fast user switching with two people** (installation for the whole Mac: the 0.22.0 package without an MDM, preferences written with `defaults`): the package took over m1's own installation (same ID); m1, then nvtest (from the lock screen's list of users), then m1 again: the daemon ended the agent of the one who left and handed the ID to the one in front each time (uid 501, 503, 501; the first start in nvtest's new session failed and was retried 11 s later); the viewer saw nvtest's desktop, with the macOS request above for the new person.
- The language choice said "Windows language" on macOS: now "system language" (5 languages).
- **Limits from a Pro server** (a Pro test server built for macOS arm64 from nuvira 0.22.0 and run on the Mac itself, with the test licence and the certificate of the LAN test server, whose single seat it took; agent of the branch and the 0.22.0 viewer on the Mac): rules changed through the console API (`PUT /console/api/policy`). Set during a session, the agent got them at once ("rules of the server … no_files=true …") and the open session kept its permissions, as documented ([32-session-permissions.md](32-session-permissions.md): changes hold from the next session); the next session started with clipboard, files, terminal, tunnels, printing and resolution off, and a download was refused ("the rules of the server do not allow file transfers"); rules cleared, the next session had everything again and the download worked.

## Tests of 2026-11-13 on a Mac mini M1 (macOS 27.0, release 0.25.0)

A Scaleway Mac mini M1 with nothing of NuviraHub on it; the release's `nuvira-macos.pkg` installed for the whole Mac with settings written first (`Server`, `ServerKey`, `PasswordHash` in `/Library/Preferences/io.github.nuvira-hub.nuvira`), see [35](35-macos-deployment.md).

- **Permissions on macOS 27.** The first session got the agent's translated refusal ("the Mac has not allowed screen sharing yet…") while macOS showed its Screen Recording request; allowed in System Settings (the administrator's password asked). The list that was "Accessibility" is now called **Device Control and Data Access** in Privacy & Security: empty until NuviraHub is added with "+". Once both were allowed, macOS 27 asked a third time, at the first capture: **"nuvira" is requesting to bypass the system private window picker and directly access your screen and audio**, Allow / Open System Settings: after Allow the sessions went on with no more questions. Someone must be at the Mac (or use Screen Sharing) for these three the first time; an MDM grants the first two with the profile's PPPC, the third was not tried with an MDM (macOS 15.1 added a restriction for it, `forceBypassScreenCaptureAlert`: to check before the 1.0 notes say anything).
- **The driver extension** (Karabiner's virtual keyboard, for the sign-in screen) asked to be allowed in Login Items & Extensions right after the package: left for later, not needed for these tests.
- **Shared sessions** ([39](39-shared-sessions.md)), the Mac as the controlled computer: the NUC's viewer first, the web client of NuviraHub Pro joined; the guest saw the Mac's 1920x1080 desktop and moved the Mac's pointer (read with `NSEvent.mouseLocation` over SSH). With the app open, its strip read "PC-STUDIO-NUC and NuviraHub Web are controlling this computer · direct connection".
- **Sound in recordings** ([38](38-remote-audio.md#in-recordings)): the NUC's viewer recording the Mac while it played the test tone with `afplay`: an Opus track next to the 1920x1080 video, 451 packets, none lost. The Mac as the viewer, recording PC-LABORATORIO playing the three test tones: the MP4 on the Mac has video 1280x1024 and the Opus track with the three tones complete, at their three levels.
- **Cmd from a Mac to a Mac**: the viewer's side runs on macOS in the CI at every build (test `mac_to_mac_keeps_cmd`: towards a Mac, Cmd goes as Cmd and Control as Control; towards the others they are swapped), and the agent's side was tried on 2026-11-08 (Cmd from the Windows key into the Finder). A Mac towards itself sends the injected keys back into its own viewer window, so a test with two real Macs is still to do.

## Not tested yet## Not tested yet

Single sign-on; Mac to Mac (Cmd kept: needs a second Mac); several displays, a Retina/5K screen; a real MDM.
