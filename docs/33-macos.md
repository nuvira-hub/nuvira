# NuviraHub — macOS

[← Index](README.md)

Since 0.15.0 the app runs on macOS 13 or later (one universal `.app` for Apple silicon and Intel, in a `.dmg` signed with a Developer ID and notarized). It controls other computers and a Mac can be controlled. Unattended access works as a launchd LaunchAgent (below). Not released yet. Automatic updates exist only for Windows for now.

## Package

`.github/workflows/client-macos.yml` (runner `macos-26`, for the ScreenCaptureKit SDK) builds both architectures, joins them with `lipo` and signs and notarizes them with `tools/package-macos.sh`. The output is `nuvira-macos.dmg`. Bundle identifier: `io.github.nuvira-hub.nuvira`.

## Controlled Mac

- **Screen**: ScreenCaptureKit (`client/capture/src/macos.rs`), BGRA frames with the cursor, at the display's pixel size (2× on Retina).
- **Mouse and keyboard**: Quartz events posted at the HID level (`client/input/src/macos.rs`).
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

## Not tested yet

Everything in this section on a real Mac: the LaunchAgent (install, remove, after a restart with automatic login, Login Items switch), the remote restart, the keyboard flags and the ISO swap. Also clipboard, files, terminal, tunnels and printing in both roles. The list is kept in the internal parity document.
