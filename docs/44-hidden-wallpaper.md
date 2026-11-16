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
  - macOS: `NSWorkspace.setDesktopImageURL` with a 1x1 black picture for every
    screen, then each screen's own picture back. It must be called from a main
    thread, which the agent's is not: the program is started again as a helper
    for each step (`--nuvira-wallpaper get|hide|restore`), as the black screens
    of privacy mode. Only the current Space of each screen changes.

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

On the Mac mini M1 (Scaleway, macOS 27.2.1, 2026-11-16; app built there from the branch, signed with a fixed test identity): PC-LABORATORIO's viewer with `--hide-wallpaper` to the
LaunchAgent's agent: "wallpaper hidden for the session" (the helper process), the
desktop black on the Mac's screen, "wallpaper put back" at the end, the picture
back.

Not tried yet: several Spaces and several displays on the Mac (one display here).