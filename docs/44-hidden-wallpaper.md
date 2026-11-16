# NuviraHub — Wallpaper hidden during the session

[← Index](README.md)

The technician can have the controlled computer's wallpaper hidden while the
session lasts: a plain background instead of a photo compresses much better, which
helps on slow connections. The picture is put back when the session ends, and an
agent that was killed puts it back when it starts again (the state file
`wallpaper.txt` next to the device key keeps what was there).

- **Technician**: app, Settings, "My sessions", "Hide the wallpaper of the
  computers I control" (the session window gets `--hide-wallpaper`).
- **Protocol**: `Hello.wallpaper` (agent, field 29) and `SetWallpaper { hidden }`
  (viewer → agent, field 45), sent when the session starts.
- **Agent** (`nuvira_capture::wallpaper`):
  - Windows: `SystemParametersInfo(SPI_SETDESKWALLPAPER)` with no picture,
    without writing the profile; also from the service's agent (SYSTEM), which
    runs on the person's desktop.
  - GNOME and Cinnamon: `picture-options` of `org.gnome.desktop.background` or
    `org.cinnamon.desktop.background` set to `none`, then back.
  - KDE Plasma: a Plasma script (`org.kde.PlasmaShell.evaluateScript`, through
    `busctl`) switches every desktop to the plain colour wallpaper
    (`org.kde.color`, black), then back to its own plugin.
  - macOS: black windows at the wallpaper's level (`kCGDesktopWindowLevel` + 1,
    below the desktop's icons and every window), on every screen and every Space
    (`canJoinAllSpaces`), held by a helper (`--nuvira-wallpaper cover`, this
    program started again: AppKit wants a main thread, which the agent's is not).
    The helper follows the screens (checked every 2 s) and ends when the agent
    closes its standard input: at the end of the session, or with the agent if it
    is killed. Nothing of the person's settings changes, so nothing has to be put
    back. The first version changed the picture (`setDesktopImageURL`): see the
    tests of 2026-11-16 for why it was dropped; a picture left by it is still put
    back (`--nuvira-wallpaper restore`).

Only in sessions through a server, with a screen (not terminal-only, not guests).

## Tests of 2026-11-15

| Computer | How it was checked | Result |
|---|---|---|
| PC-LABORATORIO, test agent | `SystemParametersInfo(SPI_GETDESKWALLPAPER)` in the person's session before, during, after | picture → none → the same picture |
| PC-LABORATORIO, test agent killed during the session | same, after starting it again | left hidden, put back at the start ("left hidden by an earlier session") |
| PC-LABORATORIO, installed service (SYSTEM) | same | picture → none → picture |
| Mint 22.3, Cinnamon | `gsettings get org.cinnamon.desktop.background picture-options` | 'zoom' → 'none' → 'zoom' |
| Ubuntu 26.04, GNOME Wayland | `org.gnome.desktop.background picture-options` | 'zoom' → 'none' → 'zoom' |
| Kubuntu 26.04, KDE Plasma Wayland | the desktops' `wallpaperPlugin` | org.kde.image → org.kde.color → org.kde.image |
| The app's setting (PC-LABORATORIO, UI Automation) | `app.json` | `hide_wallpaper: true` |

## Tests of 2026-11-16 on the Mac mini M1 (Scaleway, macOS 27.2.1, 2026-11-16)

The app built there from the branch, signed with a fixed test identity;
PC-LABORATORIO's viewer with `--hide-wallpaper` to the LaunchAgent's agent.

First version (`setDesktopImageURL`):
- one Space: the desktop black, the picture back at the end;
- two Spaces (a second one added in Mission Control): **Desktop 1 black, Desktop 2
  still showed the photo** (also to the technician after switching Space);
- the session ended while Desktop 2 was in front: "wallpaper put back" in the log,
  but the picture went back on Desktop 2 and **Desktop 1 stayed black for good**
  (System Events: `nuvira-black.png` still set; put back by hand).

So the picture is not touched any more (the black windows above). With them:

| Test | Result |
|---|---|
| Two Spaces, session from LAB | Desktop 1 and Desktop 2 black; LAB's window shows the black background, icons and windows still there |
| End of the session (90 s) | windows gone on both Spaces, the helper ended, the picture (System Events) still `DefaultDesktop.heic` |
| The agent killed (`kill -9`) during the session | the helper ended at once (end of its input), the black gone; the agent started again by launchd: "put back" (nothing to do) |
| A second display added during the session (`CGVirtualDisplay`, 1280x720, the private API Chromium uses in its tests) | within 2 s new windows on both displays (1920x1080 and 1280x720), the old one closed |
| A test agent streaming the second display (`--display 1`) | LAB's screenshot: the second display's menu bar on black; without `--hide-wallpaper` the photo |

Not tried: real displays (only the virtual one), full-screen apps' Spaces (the
windows are not `FullScreenAuxiliary`: a full-screen app has no wallpaper to
hide).

Not tried yet: several Spaces and several displays on the Mac (one display here).