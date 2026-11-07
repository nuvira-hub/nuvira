# NuviraHub — Linux

[← Index](README.md)

Since 0.13.0 the app runs on Linux and controls other computers: ID, address book, connecting, the session window with its toolbar. Since 0.14.0 a Linux computer with a **Wayland** session (the default on Ubuntu and Fedora) can also be controlled: screen, mouse, keyboard, clipboard, files. After 0.14.0 an **X11** session can be controlled too (Linux Mint, Xfce): see below; not released yet. Unattended access works as a systemd user service (below); automatic updates exist only for Windows for now.

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

## Controlled computer on X11

First version, tested on Linux Mint 22.3 Cinnamon. `client/capture/src/x11.rs` reads the root window with `GetImage` (no shared memory yet) and draws the cursor from XFixes on top; a still screen is detected by comparing the image and the pointer position with the previous ones. `client/input/src/x11.rs` injects with XTest: X key codes are the Linux ones plus 8, the wheel is clicks of buttons 4–7. The backend follows the session: `WAYLAND_DISPLAY` → portal, otherwise X11. No permission dialog on X11.

## Hardware encoder (VA-API)

`client/codec/src/vaapi.rs`, branch `linux-privacy-vaapi`: H.264 on Intel (iHD, i965) and AMD (Mesa) graphics cards through VA-API, with libva loaded at run time (`libva.so.2`, `libva-drm.so.2`, present on every desktop distribution): no other package needed, and without it the agent uses OpenH264.

- **Device:** the first render node (`/dev/dri/renderD128`…) whose driver encodes H.264; `NUVIRA_VAAPI_DEVICE` picks one. The user needs access to it (group `render`, normal for the desktop user).
- **Encoding:** profile Main (CABAC), else High, else Constrained Baseline; slice encoding, else low-power (newer Intel). P frames referring only to the previous frame, keyframes when asked, one frame out for every frame in. CBR (VBR where missing); the adaptive bitrate sends a new rate-control parameter with the next frame.
- **Headers:** written by the driver (no packed headers). At start a test frame must carry SPS, PPS and an IDR slice, otherwise the encoder is refused and OpenH264 is used.
- **Input:** BGRA → NV12 BT.601 limited on the CPU (as for the other hardware encoders), copied into the surface with `vaDeriveImage`, or `vaPutImage` where surfaces cannot be mapped.
- **Failure during a session** (screen larger than the driver allows, GPU reset): the encoder carries on with OpenH264 for the rest of the session.
- **Choice:** with `--encoder auto` it is measured against OpenH264 at start, as on Windows.
- **Structures** written by hand and checked against the C headers of libva 2.17 (sizes and offsets in compile-time asserts).
- **Tested:** on the VMs (no GPU) the driver refuses and OpenH264 is used. Not yet on a real graphics card: the WSL of the NUC (Intel UHD, Comet Lake) has no D3D12 video encoding, so Mesa's d3d12 VA driver does not start. NVENC: not done.

## Unattended access

A systemd **user** service, `nuvira-agent.service`, runs the agent as the person's own user (not root), with the permanent password (its hash in `~/.config/nuvira/service.env`, mode 0600). The app installs and removes it from Settings → Unattended access, or `nuvira service install [--server HOST] [--password-file FILE]` and `nuvira service uninstall` (`client/service/src/user.rs`).

- **Start.** An autostart entry (`~/.config/autostart/nuvira-agent.desktop`) runs `nuvira service start` at login: it imports the desktop session's variables (display, Wayland, D-Bus) into systemd and restarts the service, so the agent can read the screen. systemd restarts it if it ends.
- **App.** With the service installed the app runs no agent of its own: it connects to the service's one through a Unix socket in `$XDG_RUNTIME_DIR/nuvira/` (folder 0700), and each side accepts only a process of the same user (`SO_PEERCRED`). It shows the service's ID and temporary password; the permanent one is accepted without confirmation.
- **Wayland.** GNOME's permission is asked when installing, while the person is there, and the service then uses it (no dialog, also after a restart). GNOME ties a saved permission to the program that asked for it, telling programs apart by their systemd unit: asked from the app it was refused to the service (Fedora, 2026-11-06). So it is asked by `nuvira service portal`, started through `systemd-run` like the service.
- **Limits.** Nothing runs before someone logs in: after a restart the computer needs automatic login. On GNOME with Wayland the screen must not lock, since GNOME refuses to share it while locked. The login screen itself cannot be reached.

## Local address book

On Windows the saved passwords are protected with DPAPI. On Linux they are encrypted with AES-256-GCM, with a random key in `book.key` (mode 0600) in the data folder (`~/.config/nuvira`): it keeps them out of backups and synced folders of the address book alone, not from someone who can read the user's files.

## Tests of 2026-11-02

**Controlled Linux computer** (Ubuntu 26.04 VM, viewer on a Fedora 44 VM, both GNOME 50 on Wayland): screen at 1280×800 (openh264), mouse, keyboard with accents, wheel, clipboard with text and images both ways, files both ways (3 MB, same SHA-256); the saved permission skips the dialog; sharing resumes by itself after locking and unlocking the screen. Also from app to app through the public server, with NuviraHub's confirmation.

**Controlled Linux on X11** (2026-11-03, Mint 22.3 Cinnamon VM, from Fedora): mouse, keyboard with accents, wheel; about 25 fps while things move (software encoding 17 ms per frame), almost nothing on a still screen.

**Linux technician:** Ubuntu 26.04 (GNOME on Wayland, VM without GPU) controlling the NUC (Windows 11): direct path, 1920×1080, median latency 6 ms, decode 2.4 ms; the archive and the `.deb` from the workflow install and start, with their icons in the menu and in the dash.

## To do

- **X11, faster capture**: MIT-SHM and XDamage instead of `GetImage` and comparing images.
- **Unattended access, beyond the user session**: the login screen and a locked GNOME screen on Wayland; the way is probably Mutter's remote desktop API, as gnome-remote-desktop uses it.
- **Automatic updates**: signed apt and dnf repositories; the `linux-x64` channel for the archive.
- **Several screens on Wayland**: one shared screen, chosen in the system dialog.
- **Other desktops**: tested with GNOME only; KDE Plasma to be tested; Sway and Hyprland have no remote desktop portal (screen without input).
- **Performance**: DMA-BUF capture, the cursor as metadata, NVENC; VA-API to be tested on real graphics cards.
- **Ubuntu 22.04 and Mint 21**: dropped in 0.14.0 (PipeWire too old); possible by loading PipeWire at run time.

**Unattended access** (2026-11-06): the user service on Ubuntu 26.04 and Fedora 44 (Wayland) and Mint 22.3 (X11) starts at login and is reached with the permanent password with no app open; after a restart with automatic login both came back by themselves, and on Wayland the saved permission was accepted without a dialog. The app links to the service's agent and shows its ID.
