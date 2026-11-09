# NuviraHub — Remote resolution adapted to the window

[← Index](README.md)

During a session the screen of the controlled computer can take the size of the technician's window, as Remote Desktop does: no shrinking, no black bands, text at its real size. At the end of the session the screen goes back to its own resolution.

- **From the viewer.** The toolbar button with a screen and two corners turns it on and off; it appears only when the agent can do it and the session allows it. It starts off (choice of 2026-11-09: whoever sits at the remote computer should not see the screen change unless the technician wants it); the viewer remembers the last choice (`viewer.json` in its data folder, `adapt_resolution`). `nuvira-viewer --adapt-resolution` turns it on for one session (tests).
- **From the web client.** Button "Adapt resolution" in the toolbar of the page, remembered by the browser (local storage). The area is the page's drawing area in device pixels; full screen in the page counts too.
- **When the window changes.** The request leaves once the window has kept its size for 0.8 s (while it is dragged the remote screen would change at every step). Changing screen (1, 2, …) puts the previous one back and adapts the new one; with "All" the resolution is not changed. A minimised window asks for nothing.
- **What the screen becomes.** A screen takes only its own modes (a real monitor has a few: 1280x1024, 1280x720, 1024x768, …), so "the window's size" becomes the mode that fills the window best: among the modes at most 15% larger than the window, the one that shows the most pixels once fitted in it, a shrunk mode counting a little less, then the shape closest to the window's (`nuvira_capture::resolution::choose`). With few modes the one just larger is usually better than the largest that fits: for a 1280x720 window on Mint's virtual screen, 1360x768 (shrunk by 6%) rather than 800x600. Modes below 800x600 are left out when the screen has larger ones. If the best mode is the current one nothing changes.
- **Back.** The screen goes back to its own resolution when the technician turns the button off, when the person at the computer switches it off, when the session ends however it ends (closed, connection lost), and, if the agent itself was killed, when it starts again.
- **At the remote computer.** The strip of the app has a switch "Screen resolution" among the session permissions ([32-session-permissions.md](32-session-permissions.md)): switched off, the screen goes back at once and the viewer is told; switched on again, the viewer asks again by itself.
- **Forbidding it.** On a NuviraHub Pro server an administrator forbids it for every device with the rule "No change of the screen resolution" (web console → Rules, or app → Administration), `Policy.no_resolution`, or for single accounts and book members in their permissions. The agents then do not announce it and the button does not appear.

## Systems

| System | How | Put back if the agent is killed |
|---|---|---|
| Windows | `ChangeDisplaySettingsEx` on the screen's device (`\\.\DISPLAY1`), same colour depth, the same refresh rate when the mode has it; never `CDS_UPDATEREGISTRY`, so the mode saved in the registry stays the person's | The agent writes the changed screens in `resolution.txt` in its data folder (`C:\ProgramData\nuvira` for the service); when it starts again it puts each back to the registry mode and deletes the file |
| macOS | `CGConfigureDisplayWithDisplayMode` with `kCGConfigureForAppOnly`, among the usable modes with the same pixels per point as now (a Retina mode stays Retina) | macOS itself restores the screen when the agent process ends, however it ends |
| Linux X11 | XRandR: the CRTC of the one output turned on gets a mode of that output, the X screen is resized around it (grown first, shrunk after, as `xrandr` does), under a server grab | `resolution.txt` keeps the output's name and size; put back when the agent starts again |
| Linux Wayland | Not available: the agent does not announce it | — |

Not done: several screens turned on under X11 (one X screen holds them all: refused with "several screens turned on"), rotated screens under X11, screens captured through GDI on Windows (no device name).

After a change the agent stops the capture and opens the same screen again, as when the technician picks another screen: new video format, list of screens and input area. DXGI refuses the duplication for a moment after a mode change (`E_ACCESSDENIED`), X11 gives one `BadMatch`: the capture retries every half second and goes on by itself.

## Protocol

- `Hello.resolution` (agent): it can change the resolution (not in view-only mode, with a synthetic image, on Wayland, or with the rule).
- `SetResolution { width, height }` (viewer): the size of the drawing area in pixels; `0x0` puts the screen back.
- `ResolutionState { width, height, adapted, error }` (agent), after every `SetResolution`: the size now, whether the session changed it; `error` is an `@agent.resolution_*` key.
- `Permissions.resolution`: allowed now; `"resolution"` in `switched_off` when the person at the computer switched it off.
- `Policy.no_resolution` (server rules and session limits, bit 11 of the limits in the Pro server).

Older agents do not announce it, older viewers never send `SetResolution`: nothing changes for them. One session at a time changes a screen: a second one is refused with "another session has already changed the resolution of this screen".

## Tests of 2026-11-09

On PC-LABORATORIO (Windows 10, a 1280x1024 monitor with modes up to that), Mint 22.3 X11 (virtual screen, many modes), the NAS server and the Pro test server on the Ubuntu VM:

- Tool `cargo run -p nuvira-capture --example resolution` (list, fit, leave, recover) on PC-LABORATORIO and Mint: modes read, changed and put back; a run left changed was put back by `recover`.
- Agent on PC-LABORATORIO in the user's session, viewer on Mint with `--adapt-resolution`: 1280x1024 → 1280x720 for a 1280x720 window; window made 1100x728 and maximised: the remote screen followed each time; viewer closed: back to 1280x1024 when the session ended. Agent killed during a session: the screen stayed at 1280x720, and the agent started again put it back at once.
- The same with the installed service (agent as SYSTEM in the user's session, temporary password accepted in the app): changed, then the agent process killed: the service started it again and it put the screen back.
- Agent on Mint, viewer on PC-LABORATORIO: 1280x800 → 1360x768 for a 1280x720 window (after the first tests 800x600 was chosen: the rule then took the largest mode that fits, changed to the one above); the toolbar button clicked off: back to 1280x800.
- Switch of the person at the PC (local channel of the agent, `{"command":"switch","name":"resolution","on":false}`): back to 1280x800 at once, the viewer said "Switched off at the remote computer: Screen resolution"; on again: 1360x768 again without touching the viewer.
- Rule "No change of the screen resolution" set through the console API of the Pro test server: the agent received it while connected; the next session had no button and the screen stayed as it was.
- Web client in Chrome on PC-LABORATORIO towards Mint, through the Pro test server: off at the start; button on with a page of about 1264x840: "Remote screen at 1280x800" (already the best mode); full screen in the page (1280x985): 1280x960; button off: back to 1280x800.

Not tried yet: macOS (needs a Mac: the modes, Retina, the restore by macOS when the agent ends), a Windows PC with two monitors, an NVIDIA or AMD driver on Windows, a laptop panel (few modes, often only the native one).
