# NuviraHub — Privacy mode

[← Index](README.md)

During a session the technician can turn the screen of the controlled PC black and block its own mouse and keyboard, while the viewer keeps showing the real screen. Use it when the person at the PC should not see what is being done (credentials, other customers' data) or when nobody should touch the PC during the work.

- **From the viewer.** The toolbar button with a slashed screen turns it on and off; it appears only when the agent has privacy mode. `nuvira-viewer --privacy` turns it on as soon as the session starts (automated tests). The viewer shows a notice when it turns on, off, or fails, with the reason.
- **At the PC.** Every screen is black with a short message in the local language: remote support in progress, mouse and keyboard paused until the end of the session, Ctrl+Alt+Del to lock the PC.
- **Not allowing it at the PC.** App settings → "This computer" → "Allow privacy mode" (Windows and macOS, on by default). The app saves the choice (`app.json`, `no_privacy`) and sends it to its agent, or to the service's one, at every start and at every change (`AgentCommand::AllowPrivacy`). The agent keeps its own copy in `permissions.json` in its data folder (with the service: `C:\ProgramData
uvira`, writable only by administrators and SYSTEM), so the choice holds also while the app is closed. Not allowed: the agent does not announce privacy mode, refuses it with `@agent.privacy_not_allowed`, and turns it off at once if it was on.
- **Forbidding it.** On a NuviraHub Pro server an administrator can forbid it for every device: rule "No privacy mode" in the web console (Rules) or in the app (Administration), `Policy.no_privacy`. The agents then do not announce it, the button does not appear, and a `SetPrivacy` is refused with `@agent.privacy_disabled`. Like the other rules it applies from the next session.
- **End.** Privacy mode ends when the technician turns it off and always with the session (closed, connection lost, agent ended). Windows removes the windows and the input hooks by itself if the agent process dies; on macOS the black screens are a child process that ends when the agent's end of its input closes, and the event tap dies with the agent.

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

## Limits

- **Ctrl+Alt+Del** cannot be blocked by any program: the person at the PC can always lock it (said on the black screen). The **secure desktop** (UAC prompt, lock screen) is not covered: the person at the PC sees it.
- Some shell surfaces have their own layer above every topmost window and are expected to show through: the Start menu, notifications, the volume indicator (not verified yet).
- The **mouse pointer** stays visible on the black screen and moves with the technician's mouse.
- Started by the user, without nuvira-service, the hooks may not see the input going to elevated windows (UIPI); with the service (SYSTEM) this does not apply. Not verified yet.
- **macOS:** the black screens are not rebuilt when screens are added or removed during privacy mode. Keys handled below the session (power button, Touch ID, the lock key) cannot be blocked. Not tested on a real Mac yet.
- **Linux:** not available yet; the button does not appear.

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

Not tested yet: turning it off from the button or unticking the setting while privacy mode is on, two screens, an elevated window in front, privacy mode in a session of the service's agent.
