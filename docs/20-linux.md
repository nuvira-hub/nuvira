# NuviraHub — Linux

[← Index](README.md)

Since 0.13.0 the app runs on Linux and controls other computers: ID, address book, connecting, the session window with its toolbar. Since 0.14.0 a Linux computer with a **Wayland** session (the default on Ubuntu and Fedora) can also be controlled: screen, mouse, keyboard, clipboard, files. With an X11 session it cannot yet: the app says so and hides that part of the main window (`controllable()` in `client/app/src/main.rs`). Unattended access (the service) and automatic updates exist only for Windows for now.

## Packages

`.github/workflows/client-linux.yml` builds them in a Debian 12 container with `tools/package-linux.sh`, at every change to the client and at every tag. The binary needs a glibc at least as new as the one it was built with (2.36), and the PipeWire bindings need PipeWire 0.3.65 or later: building on the oldest distribution we support keeps it running on the newer ones (Debian 12+, Ubuntu 24.04+, Mint 22+, Fedora 40+). Up to 0.13.0 it was built on Ubuntu 22.04, whose PipeWire 0.3.48 is too old for the bindings. The packages are a workflow artifact; they go into the GitHub release by hand, with the signed Windows files.

| File | For | Installs into |
|---|---|---|
| `nuvira-linux-x64.deb` | Ubuntu, Debian, Mint | `/usr/bin`, `/usr/share` (dependencies from apt) |
| `nuvira-linux-x64.rpm` | Fedora | the same (dependencies from dnf) |
| `nuvira-linux-x64.tar.gz` | other distributions, no sudo | `~/.local`, with `install.sh` (`--uninstall` removes it) |

The `.deb` and `.rpm` come from `client/app/linux/nfpm.yaml` (nfpm). The names carry no version, so the website can link the latest release. Runtime needs: WebKitGTK 4.1, GTK 3, libxkbcommon-x11.

## Windows and icons

- **X11 only.** The viewer forces winit onto X11 (XWayland on a Wayland desktop): the child webviews of wry (toolbar, chat, notices) exist only on X11. `GDK_BACKEND=x11` keeps GTK on the same display; `WEBKIT_DISABLE_DMABUF_RENDERER=1` (unless set) avoids blank webviews with some drivers.
- **GTK events** are pumped from winit's loop (`about_to_wait`, every 16 ms).
- **X errors.** Xlib has a single error handler per process: winit's took GTK's errors as its own and panicked. A hook (`register_xlib_error_hook`) passes on the errors of other displays.
- **Hidden webviews.** A child webview created hidden stays mapped on X11 until GTK has run: the chat panel showed as a black box. It is hidden again after pumping GTK events. The WebKitGTK context menu, which could stay stuck on screen, is disabled.
- **Icons.** GNOME matches a window to its `.desktop` file by window class: `nuvira` for the app (`io.github.nuvira-hub.nuvira.desktop`), `nuvira-viewer` for session windows (`nuvira-viewer.desktop`, hidden from the menu), each with its own icon.

## Controlled computer on Wayland

On Wayland a program cannot read the screen or inject input by itself: it asks the system through xdg-desktop-portal. `client/portal` opens one **RemoteDesktop** session with a **ScreenCast** source (the whole screen, cursor drawn in the image): one permission gives both.

- **Permission.** The first time GNOME asks the person at the computer to allow remote control (after NuviraHub's own confirmation). The portal then gives a restore token, kept in `remote-desktop-token` (mode 0600) in the data folder: later sessions start without a dialog, until the person revokes it in the system settings. The token is never deleted: GNOME refuses it also while the screen is locked, and a new permission replaces it.
- **Screen.** `client/capture/src/linux.rs` reads the shared screen as a PipeWire stream in shared memory (BGRx, no DMA-BUF), in its own thread; buffers are reused. The size in pixels comes from the stream format (the portal gives logical coordinates).
- **Input.** `client/input/src/linux.rs` sends mouse, wheel and keys through the same session: absolute pointer in logical coordinates, Linux key codes (`keys::evdev_code`), wheel in notches (Windows' sign is the opposite). Ctrl+Alt+Del does not exist.
- **Screen locked.** GNOME closes the session when the screen locks and refuses new ones until it is unlocked. The capture asks again every two seconds with the saved permission only (no dialog), and the image resumes by itself after unlocking; the input follows the new session. A start with the saved permission gets 15 s: right after a lock the portal sometimes never answered.
- **D-Bus.** Each portal session has its own D-Bus connection. ashpd's shared one belongs to the Tokio runtime that made it (the thread of the first session): once that ended, every later request went unanswered.
- **Received files** go to the user's downloads folder of `user-dirs.dirs` ("Scaricati" on an Italian desktop).
- **Clipboard** goes through arboard (X11, through XWayland), as on the viewer side.
- **Build:** `libpipewire-0.3-dev`, `libspa-0.2-dev` and clang (bindgen); at run time `libpipewire-0.3-0`.

## Local address book

On Windows the saved passwords are protected with DPAPI. On Linux they are encrypted with AES-256-GCM, with a random key in `book.key` (mode 0600) in the data folder (`~/.config/nuvira`): it keeps them out of backups and synced folders of the address book alone, not from someone who can read the user's files.

## Tests of 2026-11-02

**Controlled Linux computer** (Ubuntu 26.04 VM, viewer on a Fedora 44 VM, both GNOME 50 on Wayland): screen at 1280×800 (openh264), mouse, keyboard with accents, wheel, clipboard with text and images both ways, files both ways (3 MB, same SHA-256); the saved permission skips the dialog; sharing resumes by itself after locking and unlocking the screen. Also from app to app through the public server, with NuviraHub's confirmation.

**Linux technician:** Ubuntu 26.04 (GNOME on Wayland, VM without GPU) controlling the NUC (Windows 11): direct path, 1920×1080, median latency 6 ms, decode 2.4 ms; the archive and the `.deb` from the workflow install and start, with their icons in the menu and in the dash.
