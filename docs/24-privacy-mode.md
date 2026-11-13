# NuviraHub — Privacy mode

[← Index](README.md)

During a session the technician can turn the screen of the controlled PC black and block its own mouse and keyboard, while the viewer keeps showing the real screen. Use it when the person at the PC should not see what is being done (credentials, other customers' data) or when nobody should touch the PC during the work.

- **From the viewer.** The toolbar button with a slashed screen turns it on and off; it appears only when the agent has privacy mode. `nuvira-viewer --privacy` turns it on as soon as the session starts (automated tests). The viewer shows a notice when it turns on, off, or fails, with the reason.
- **At the PC.** Every screen is black with a short message in the local language: remote support in progress, mouse and keyboard paused until the end of the session, Ctrl+Alt+Del to lock the PC.
- **Not allowing it at the PC.** App settings → "This computer" → "Allow privacy mode" (Windows and macOS, on by default). The app saves the choice (`app.json`, `no_privacy`) and sends it to its agent, or to the service's one, at every start and at every change (`AgentCommand::AllowPrivacy`). The agent keeps its own copy in `permissions.json` in its data folder (with the service: `C:\ProgramData
uvira`, writable only by administrators and SYSTEM), so the choice holds also while the app is closed. Not allowed: the agent does not announce privacy mode, refuses it with `@agent.privacy_not_allowed`, and turns it off at once if it was on.
- **Forbidding it.** On a NuviraHub Pro server an administrator can forbid it for every device: rule "No privacy mode" in the web console (Rules) or in the app (Administration), `Policy.no_privacy`. The agents then do not announce it, the button does not appear, and a `SetPrivacy` is refused with `@agent.privacy_disabled`. Like the other rules it applies from the next session.
- **End.** Privacy mode ends when the technician turns it off and always with the session (closed, connection lost, agent ended). Windows removes the windows and the input hooks by itself if the agent process dies; on macOS the black screens are a child process that ends when the agent's end of its input closes, and the event tap dies with the agent. On Linux a child process does the work and undoes it when its input closes or it is told to end (see below); if both processes are killed, the agent undoes it when it starts again.

## How it works (Windows 10 version 2004 or later)

Module `nuvira_input::privacy`, a thread of its own in the agent:

- **Black windows.** One window for each screen, `WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`: always on top, never in front for the keyboard, and transparent to clicks, so the technician's clicks reach the windows below. `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` keeps them out of every capture (DXGI Desktop Duplication, GDI, screenshots). A window is shown only after `GetWindowDisplayAffinity` confirms the flag: on older builds the flag acts as `WDA_MONITOR` and the technician would see black too, so the agent refuses (`@agent.privacy_unsupported`). Every 500 ms the windows go back on top of windows that became topmost after them; when the screens change (`WM_DISPLAYCHANGE`) they are created again.
- **Local input blocked.** Low-level hooks (`WH_MOUSE_LL`, `WH_KEYBOARD_LL`) drop every event whose `dwExtraInfo` is not NuviraHub's tag (`INJECTED_TAG`), which `SendInput` in `nuvira_input::windows` puts on everything it injects. Physical mouse and keyboard, and input injected by other programs, do not reach Windows.
- **Protocol.** The agent announces it in `Hello.privacy_mode` (not in view-only mode or with a synthetic image); the viewer sends `SetPrivacy { on }` and the agent answers `PrivacyState { on, error }` (see `proto/nuvira/v1/session.proto`).

## How it works (macOS)

Module `nuvira_input::privacy` (`privacy_mac.rs`), branch `macos-privacy-vt`:

- **Black windows in a child process.** AppKit windows can only be made on a main thread, and the agent's main thread runs tokio (or Tauri inside the app). The agent starts its own program again with `--privacy-overlay` (`overlay_if_asked()`, first thing in `main` of `nuvira` and `nuvira-agent`; the message comes in `NUVIRA_PRIVACY_MESSAGE`). The child is an accessory app (no Dock icon) with one borderless black window per screen at `CGShieldingWindowLevel` (above the menu bar, the Dock and full-screen apps; only the pointer is higher), `ignoresMouseEvents`, on every Space, `NSWindowSharingNone`. It prints `ready <window numbers>` and ends when its standard input closes.
- **Left out of the capture.** The windows start transparent (alpha 0). The agent passes the child's pid to `nuvira_capture::hide_process`: the ScreenCaptureKit capture rebuilds its filter between two frames with `excludingApplications` for that process (`SCStream.updateContentFilter`) and confirms; the agent waits at most 1.5 s, then sends `show` and the windows turn black. A capture started later (screen switch) builds its filter with the hidden processes already in it.
- **Local input blocked.** An active event tap at session level, on a thread with its own run loop, drops every mouse and keyboard event whose `kCGEventSourceUserData` is not NuviraHub's tag; `QuartzInjector` puts the tag on everything it posts. When macOS turns the tap off (timeout, secure input) it is turned on again. Creating the tap needs the Accessibility permission, which remote control needs anyway: without it privacy mode is refused with `@agent.privacy_permission_mac`.

## How it works (Linux, X11)

Module `nuvira_input::privacy` (`privacy_x11.rs`), branch `linux-privacy-vaapi`. X11 has no way to leave one window out of a capture of the root window, so there are no black windows and no message:

- **Black screen.** The gamma ramp of every CRTC (RandR) goes to zero. The capture (`GetImage`) reads the frame buffer before the ramp, so the technician sees the real image while the monitor and the hardware pointer show black. The ramps in use before are kept and put back at the end; a ramp already at zero at the start (left by a killed process) is put back linear.
- **Local input blocked.** Every physical slave device (mice, keyboards, touchpads, the power button) is switched off with the XInput 2 property "Device Enabled" = 0, as `xinput disable` does. The XTest devices, through which the agent injects (property "XTEST Device"), stay on.
- **Child process.** The agent starts its own program again with `--privacy-overlay`; the child applies all this, prints `ready devices=N screens=M`, and every 500 ms applies it again (devices plugged in meanwhile, a night-light tool changing the gamma). It undoes everything when its standard input closes (privacy off, session over, agent ended or killed) or on SIGTERM, SIGINT or SIGHUP (systemd stopping the user service sends SIGTERM to the whole group). The switched-off devices are listed in `$XDG_RUNTIME_DIR/nuvira-privacy<display>`, removed once restored: if child and agent are both killed, the next agent start switches them on again (`privacy::recover`).
- **Offered** only in an X11 session with XInput 2 and at least one CRTC with a gamma ramp; refused otherwise with `@agent.privacy_unsupported_linux`.
- **Wayland:** see the next section.

## How it works (Linux, Wayland)

Module `nuvira_input::privacy` (`privacy_wayland.rs`) and `nuvira_input::local_input` (`local_input_linux.rs`), branch `wayland-privacy`. The same child process (`--privacy-overlay`) as on X11, with two other means:

- **Black screen.** The gamma ramp of every active CRTC goes to zero through Mutter's D-Bus API (`org.gnome.Mutter.DisplayConfig`, `GetResources`, `GetCrtcGamma`, `SetCrtcGamma`). Mutter's screencast reads the frame buffer, before the ramp. Only GNOME has this API: KDE Plasma uses black windows instead (next section); other compositors are not offered privacy mode. No message and no window, since Mutter's screencast cannot leave a window out.
- **Local input blocked by root.** On Wayland the compositor reads `/dev/input` itself, and no client can hold the input back: a GNOME Shell extension's event filter was tried and runs after Mutter has given the events to the windows and run its shortcuts (Super still opened the overview). Root can: the sign-in screen's system service (`nuvira service signin-run`, installed with the system part of unattended access) listens on `/run/nuvira-input.sock`. The child writes `block`; the service grabs every `/dev/input/event*` device with keys, relative or absolute axes (`EVIOCGRAB`: the kernel then gives its events only to that descriptor, so neither Mutter nor logind receives them), answers `ok N`, looks for new devices every 500 ms, and lets them go when the connection closes: privacy off, child ended or killed (the kernel drops the grabs with the descriptors). The technician's input is not affected: Mutter's remote desktop devices are virtual, with no `/dev/input` node. Switches (lid) and devices named "NuviraHub…" are left alone.
- **Who may ask.** Only the user of the active session of `seat0` (`SO_PEERCRED` against logind), so nobody else signed in can take the input of the person at the computer; otherwise `@agent.privacy_not_active_user`. When the active session changes (user switching) the devices are let go.
- **Offered** in a Wayland session with Mutter, a CRTC with a gamma ramp and the system part installed; refused otherwise with `@agent.privacy_unsupported_linux` or `@agent.privacy_needs_system_linux`. `NUVIRA_PRIVACY_NO_BLACK=1` on the agent offers it without the black (tests on virtual machines, which have no ramp).
- **A killed child**: the devices are free at once; a screen left at zero gets a linear ramp from `privacy::recover` (agent start, end of privacy mode).

## How it works (Linux, KDE Plasma)

Module `nuvira_input::privacy` (`privacy_kde.rs`), branch `kde-privacy`. Plasma 6.6 or later on Wayland (KWin's version read from `org.kde.KWin.supportInformation`); the same child process (`--privacy-overlay`) and the same root service for the local input as on GNOME. Unlike GNOME and X11 there is no gamma: as on Windows and macOS, black windows that the capture leaves out.

- **Black windows.** The child opens a full-screen black window on every output as a Wayland client (xdg-shell, app id `dev.nuvirahub.privacy`, a zeroed XRGB `wl_shm` buffer, an empty input region so the technician's clicks go to the windows below). A KWin script loaded first through `org.kde.kwin.Scripting` (`nuvirahub-privacy`) gives them KWin's `excludeFromCapture` (new in Plasma 6.6: the screencast draws the desktop underneath), `keepAbove` (watched: set again if it drops), skip taskbar, pager and switcher, all desktops, and gives the focus back to the window the technician was using.
- **The screencast already running.** Set as the windows appear, the exclusion holds for screencasts started afterwards but not for the technician's, which was already running: KWin 6.6 left the windows in it. A second script (`nuvirahub-privacy-shown`), run once the windows are drawn and again a second later, sets the property off and on while they are shown; that stream then leaves them out. Run again when a window opens for a screen plugged in.
- **Show desktop.** "Show desktop" (the panel's "Peek at desktop", Meta+D) hides every window, the black ones too. The child listens for `org.kde.KWin.showingDesktopChanged` and calls `showDesktop(false)` as soon as it turns on.
- **A killed child**: KWin removes the windows with the client and the root service lets the devices go; nothing to undo. `privacy::recover` unloads the scripts.
- **Offered** in a KDE Plasma 6.6 session on Wayland with the system part installed; otherwise as on GNOME (`@agent.privacy_unsupported_linux`, `@agent.privacy_needs_system_linux`).

## Limits

- **Ctrl+Alt+Del** cannot be blocked by any program: the person at the PC can always lock it (said on the black screen). The **secure desktop** (UAC prompt, lock screen) is not covered: the person at the PC sees it.
- Some shell surfaces have their own layer above every topmost window and are expected to show through: the Start menu, notifications, the volume indicator (not verified yet).
- The **mouse pointer** stays visible on the black screen and moves with the technician's mouse.
- Started by the user, without nuvira-service, the hooks may not see the input going to elevated windows (UIPI); with the service (SYSTEM) this does not apply (verified on 2026-11-10, see Tests). Without the service: not verified yet.
- **macOS:** the black screens are not rebuilt when screens are added or removed during privacy mode. Keys handled below the session (power button, Touch ID, the lock key) cannot be blocked. Not tested on a real Mac yet.
- **Linux (X11):** no message on the black screen (the gamma is zero). Drivers without a working gamma ramp keep the image on the monitor: virtual machines (virtio-gpu reports a ramp but ignores it) and some DisplayLink adapters; the local input is blocked anyway. A device switched on again from another program (`xinput enable`) is switched off within 500 ms. Laptop backlight is not touched. **Linux (Wayland):** GNOME and KDE Plasma (below), with the system part of unattended access; the black screen not seen on a real monitor yet (the test VMs have no gamma ramp). If the child process is killed the agent does not notice: the viewer still shows privacy mode on, with the local input free again (also on X11).
- **Linux (KDE Plasma):** Plasma 6.6 or later. Notifications and on-screen displays (KDE's own "remote control session started" notice, volume) are in KWin layers above the windows and show on the monitor; the window switcher and the overview (Meta+W) were not tried. No message on the black screen. With the monitor off (power saving) KWin sends the screencast no picture: the technician sees black until the screen wakes, as without privacy mode.

## Tests

2026-11-06, agent on PC-LABORATORIO (Windows 10 22H2, build 19045, 1280x1024) through the NAS server, viewer on the NUC with `--privacy`. A probe started in the user's session of PC-LABORATORIO before, during and after privacy mode:

| | before | during | after |
|---|---|---|---|
| Black windows (class `NuviraPrivacy`) | none | 1, 0,0–1280,1024, topmost, click-through, affinity `0x11` | none |
| Window under the centre of the screen | console | console (clicks pass through) | console |
| USB mouse jiggler: cursor positions in 4 s | 3 | 1 | 3 |
| Mouse and Shift injected without the tag | received | blocked | received |
| Mouse and Shift injected with the tag | received | received | received |
| GDI screenshot, mean brightness | 61 | 66 (the desktop, not black) | 66 |

The viewer window (PrintWindow of that window only) showed the real desktop during privacy mode, with the button on: DXGI capture does not see the black windows either. The agent log shows privacy mode turned off at the end of the session.

Same day, from Linux: the viewer on the Ubuntu 26.04 VM (Wayland) with `--privacy` towards PC-LABORATORIO gave the same results as the "during" column. The agent on the same VM announces no privacy mode: the NUC viewer with `--privacy` reported it as not available, without the button, and the session went on normally.

Rule "No privacy mode", same day, with the Pro test server on the Ubuntu VM and the agent on PC-LABORATORIO: turned on from the console API, the agent received it while connected (log "rules of the server … no_privacy=true"); the next session with `--privacy` found privacy mode not available and no black window was created. Turned off again: the following session turned it on.

Setting "Allow privacy mode", same day, on PC-LABORATORIO with this build installed as the service: unticked through UI Automation, the app saved `no_privacy`, the service's agent logged "privacy mode allowed by the person at the PC allow=false" and wrote `{"privacy":false}` in `C:\ProgramData
uvira\permissions.json`; ticked again, `{"privacy":true}`. An agent started with a `permissions.json` that does not allow it (through the Pro test server) was reached with `--privacy`: privacy mode not available.

Elevated window, 2026-11-10: PC-LABORATORIO with the service (SYSTEM) of 0.22.0, viewer on the Mint VM with `--privacy`. A probe started as administrator in mrbla's session (scheduled task, highest level) opened Notepad, so elevated too, brought it in front and sent three key presses with `SendInput` without NuviraHub's tag, as a physical keyboard would:

| | before | during | after |
|---|---|---|---|
| Black windows (`NuviraPrivacy`) | 0 | 1 | 0 |
| Elevated Notepad in front | yes | yes | yes |
| Keys received of 3 | 3 | 0 | 3 |

Not tested yet: turning it off from the button or unticking the setting while privacy mode is on, two screens, an elevated window with the agent started by the user (no service).

Linux, 2026-11-07, agent on the Mint 22.3 VM (Cinnamon, X11, through the Pro test server), viewer on the Fedora 44 VM with `--privacy --input-test`. The VM's own mouse and keyboard were driven through its VNC console on the NAS (QEMU USB tablet), the pointer read with `XQueryPointer`:

| | before | during | after |
|---|---|---|---|
| Gamma ramp (1024 entries), middle value | 32799 | 0 | 32799 |
| Physical devices (tablet, 2 VMware mice, AT keyboard, power button) | on | floating, "Device Enabled" 0 | on |
| Pointer moved through the VNC console | moves | does not move | moves |
| Pointer moved by the viewer (XTest) | moves | moves | moves |

The viewer's last frame showed the real desktop. The monitor of the VM (VNC screenshot) did not turn black: virtio-gpu ignores the ramp, as expected. Robustness: agent killed with `kill -9` during privacy mode → the child restored everything within 2 s; child and agent both killed → devices stayed off until the agent was started again, which logged "privacy mode of a killed process undone devices=[6, 7, 8, 9, 10]" and switched them on; SIGTERM to the agent's process group → "ended by a signal", everything restored.

Linux Wayland, 2026-11-08, agent on the Fedora 44 VM (GNOME 50) as the user service with the system part installed and `NUVIRA_PRIVACY_NO_BLACK=1` (virtio-gpu: `GetCrtcGamma` gives empty ramps), viewer on the NUC (Windows) through the NAS server with `--privacy --type-keys xyz`. The computer's own keyboard was a uinput device made by a root script ("fake-local", typing "a" every 2 s), the keys read by a full-screen GTK 4 window:

| | before | during | after |
|---|---|---|---|
| "a" of the local keyboard | 5 of 5 reached the window | 0 of 7 | 8 of 8 |
| "xyz" of the viewer | | reached the window | |
| Devices grabbed by the service | none | 6 (QEMU tablet, 2 VMware mice, AT keyboard, power button, fake-local) | none |

Robustness: the child killed with `kill -9` during privacy mode → the service let the devices go in the same second and the local keys reached the window again; a `block` request as root and as `nobody` → `error @agent.privacy_not_active_user`.

Linux KDE Plasma, 2026-11-13, agent of the branch on the Kubuntu 26.04 VM (Plasma 6.6.6, KWin 6.6.6, Wayland) as a user unit with a test password, the system part installed by the 0.22.6 package; viewer on PC-LABORATORIO through the NAS server with `--privacy --click-at 0.27,0.33 --type-keys "privacy ok\n"`. Konsole (running `cat > file`) and KCalc on the desktop, KCalc focused. What the monitor showed was read with Spectacle (KWin's screenshot, which in 6.6.6 does not leave the windows out); what the technician saw, with the viewer's snapshot:

| | Monitor | Technician |
|---|---|---|
| 1 s after privacy on | black (KDE's notice "remote control session started" above) | desktop |
| after the click on Konsole and the typing | black | desktop, "privacy ok" in Konsole, KCalc |
| after `showDesktop(true)` through D-Bus | black (turned off again: `showingDesktop` false) | |
| the file written by `cat` | | "privacy ok" |

Found on the way: the first script stopped at a line using `workspace.showingDesktopChanged` (not in KWin's scripting API), so nothing was applied and Konsole, activated by the click, went above the black window; the exclusion set as the windows appear did not reach the screencast already running (the second script fixes it, tried with a capture started before the windows: the last frame showed the desktop only after the property was set again); with the VM's monitor off (DPMS) every capture was black, privacy mode or not. The child killed with `kill -9`: no window left (counted through a KWin script), the devices released in the same second, the monitor back on the desktop.
