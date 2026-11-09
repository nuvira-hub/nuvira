# NuviraHub — Linux

[← Index](README.md)

Since 0.13.0 the app runs on Linux and controls other computers: ID, address book, connecting, the session window with its toolbar. Since 0.14.0 a Linux computer with a **Wayland** session (the default on Ubuntu and Fedora) can also be controlled: screen, mouse, keyboard, clipboard, files. Since 0.15.0 an **X11** session can be controlled too (Linux Mint, Xfce). Unattended access works as a systemd user service (since 0.16.0); since 0.17.0 also at the sign-in screen of X11 display managers, with the NuviraHub printer and automatic updates ([19-updates.md](19-updates.md)); since 0.19.0 also at GNOME's lock screen and GDM's sign-in screen on Wayland.

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
- **Headers:** SPS and PPS written by NuviraHub and given to the driver as packed headers when it takes them (as FFmpeg and GStreamer do); otherwise the driver writes them. Mesa's drivers need ours: their VA layer reads profile and level only from a packed SPS (with Mesa 26 the level stayed 0 and the D3D12 driver crashed). Slice headers are the driver's. At start a test frame must carry SPS, PPS and an IDR slice, otherwise the encoder is refused and OpenH264 is used.
- **Display:** opened once for the process and never closed: Mesa's D3D12 driver leaves a destructor for the thread that used it, and the process crashed when the thread ended after `vaTerminate` had unloaded the driver.
- **Input:** BGRA → NV12 BT.601 limited on the CPU (as for the other hardware encoders), copied into the surface with `vaDeriveImage`, or `vaPutImage` where surfaces cannot be mapped.
- **Failure during a session** (screen larger than the driver allows, GPU reset): the encoder carries on with OpenH264 for the rest of the session.
- **Choice:** with `--encoder auto` it is measured against OpenH264 at start, as on Windows.
- **Structures** written by hand and checked against the C headers of libva 2.17 (sizes and offsets in compile-time asserts).
- **Tested:** on the VMs (no GPU) the driver refuses and OpenH264 is used. On 2026-11-09 on a real GPU through WSL: PC-FEDERICO (RTX 4060 Ti, driver 610.88), Ubuntu 26.04 in WSL, Mesa 26.0.8's D3D12 VA driver (`GALLIUM_DRIVER=d3d12 LIBVA_DRIVER_NAME=d3d12`: without `GALLIUM_DRIVER` the VA driver does not find the GPU, as the WSL render node is a placeholder; this is also why the NUC's WSL failed, not its Intel UHD). Found and fixed there: the packed headers above, the display kept open, and a keyframe asked for that left a second one pending (the frame after every requested keyframe was an IDR too: on every driver). Then the test encodes an IDR and P frames that OpenH264 decodes; the bench at 1080p: 8.7 ms a frame, 2 kB, 120/120 decoded (OpenH264 4.6 ms, 4 kB), at 1440p 11.0 ms (8.7). Through WSL the frame crosses D3D12 and the CPU conversion, so these times say nothing of a native driver. Still to try: a real Intel (iHD) or AMD (radeonsi) PC with Linux installed.

## Hardware encoder (NVENC)

`client/codec/src/nvenc.rs`, branch `linux-privacy-vaapi`: H.264 on NVIDIA graphics cards with NVIDIA's own driver, which carries `libnvidia-encode.so.1` and `libcuda.so.1`; both are loaded at run time, nothing is linked and no SDK is needed. With an NVIDIA card it comes before VA-API; `NUVIRA_HW_ENCODER=vaapi` or `nvenc` keeps to one (tests).

- **API 12.0**, driver 520 or later (2022); an older driver is refused and VA-API or OpenH264 is used. The structures are byte buffers with the offsets of NVIDIA's header (nv-codec-headers n12.0.16.1, MIT), printed by `tools/nvenc-layout.c` into `client/codec/src/nvenc_layout.rs`: run it again to check them.
- **CUDA context** of its own on the first GPU, made current only around NVENC's calls.
- **Input:** the captured BGRA image as it is, into an input buffer of NVENC's: the GPU converts it to YUV (BT.601 limited, as the other encoders: checked by the test), nothing on the CPU.
- **Encoding:** preset P2 with the ultra-low-latency tuning, profile Main (CABAC), CBR with a quarter of a second of buffer, no B frames, an infinite GOP with keyframes when asked, SPS and PPS with every IDR. The adaptive bitrate reconfigures the encoder (`nvEncReconfigureEncoder`) without a keyframe. Linux has no asynchronous mode: one frame in, one out.
- **Failure during a session:** OpenH264 for the rest of it, as with VA-API.
- **Tested** on 2026-11-09 on PC-FEDERICO (RTX 4060 Ti, driver 610.88) in WSL, Ubuntu 26.04, where the libraries are NVIDIA's real ones: the test (IDR, P frames, a bitrate change, decoded by OpenH264, colour right); the bench at 1080p 3.8 ms a frame (OpenH264 4.5), 1440p 6.1 (8.8), 4K 12.7 (20.4). A whole session: the agent of the branch with `--synthetic 1920x1080@60 --encoder hardware` in WSL, the viewer on PC-LABORATORIO through the NAS server, direct connection, the bitrate rising from 4.6 to 8 Mbit/s: 59.3 fps, latency p50 26 ms, p95 35 ms, encoding 5.1–5.7 ms, no freezes, in three runs (OpenH264 in the same setup: 59.6 fps, 19 ms; the viewer of PC-LABORATORIO decodes CABAC more slowly, 6.3 ms against 2.5). The first session after WSL started had 36 fps and two freezes (the GPU waking up under WSL); not seen again. Not tried: a Linux installed on a PC with an NVIDIA card, a real screen.

## Unattended access

A systemd **user** service, `nuvira-agent.service`, runs the agent as the person's own user (not root), with the permanent password (its hash in `~/.config/nuvira/service.env`, mode 0600). The app installs and removes it from Settings → Unattended access, or `nuvira service install [--server HOST] [--password-file FILE]` and `nuvira service uninstall` (`client/service/src/user.rs`).

- **Start.** An autostart entry (`~/.config/autostart/nuvira-agent.desktop`) runs `nuvira service start` at login: it imports the desktop session's variables (display, Wayland, D-Bus) into systemd and restarts the service, so the agent can read the screen. systemd restarts it if it ends.
- **App.** With the service installed the app runs no agent of its own: it connects to the service's one through a Unix socket in `$XDG_RUNTIME_DIR/nuvira/` (folder 0700), and each side accepts only a process of the same user (`SO_PEERCRED`). It shows the service's ID and temporary password; the permanent one is accepted without confirmation.
- **Wayland.** GNOME's permission is asked when installing, while the person is there, and the service then uses it (no dialog, also after a restart). GNOME ties a saved permission to the program that asked for it, telling programs apart by their systemd unit: asked from the app it was refused to the service (Fedora, 2026-11-06). So it is asked by `nuvira service portal`, started through `systemd-run` like the service.
- **GNOME's own API.** The service's agent (`--ipc`, and the sign-in screen's) opens its sessions through `org.gnome.Mutter.RemoteDesktop` and `org.gnome.Mutter.ScreenCast` on the session bus (`client/portal/src/mutter.rs`), as gnome-remote-desktop does: no dialog, no saved permission; the portal stays the fallback (KDE, others). The app's own agent keeps the portal and its dialog.
- **Limits.** See the sign-in screen and the locked screen below.
- **For the whole computer.** An administrator can install it for everyone instead, with the computer's own ID and no sign-in first: `sudo nuvira service deploy`, or `/etc/nuvira/policy.json` before the package ([34-linux-deployment.md](34-linux-deployment.md)). The person's own installation then gives way.

## Sign-in screen

The display manager's greeter (LightDM, GDM…) is reached by a systemd **system** service, `nuvira-signin.service`, which runs `nuvira service signin-run` as root (`client/service/src/signin_linux.rs`). It is installed with the printer by `nuvira service system`, after pkexec, from the app's Install; the packages remove it and restart it after an update.

- It follows the active session of `seat0` (`loginctl`): while it is a greeter it keeps an agent there with `--signin-screen` (permanent password only), with the person's ID (device key, server fingerprint and password hash copied into `/var/lib/nuvira`, root only).
- **X11 greeters (LightDM):** the agent runs as root on the greeter's display, with the X server's own `-auth` file read from its command line.
- **Wayland greeters (GDM):** an agent as the greeter's user with its session bus, through GNOME's API, which GNOME allows there only with NuviraHub's GNOME Shell extension (see the locked screen below).
- **One agent at a time:** when the greeter appears, the user service of whoever installed it is stopped (`systemctl --user --machine=user@.host`: their systemd can outlive the session, kept by an SSH login for example, and the two agents took the ID from each other); when a person's session becomes active the greeter's agent ends and the autostart entry starts theirs.
- The program must be one only root can change: from the packages `/usr/bin/nuvira`, otherwise a copy in `/usr/local/lib/nuvira`. With SELinux (Fedora) the copy is labelled `bin_t` (`semanage fcontext`, or `chcon` without it): as `lib_t` systemd ran it as `init_t`, which may not connect to the greeter's D-Bus, and the screen at GDM was refused.

## Locked screen

- **X11** (Mint, Cinnamon): the lock screen is a window of the session's X server; captured and typed into like the desktop.
- **GNOME on Wayland:** GNOME Shell inhibits remote access whenever its session mode does not allow screencasts (`ui/main.js`, `inhibit_remote_access()` of Mutter's remote access controller): it closes every remote desktop session the moment the screen locks (`Closed` signal) and refuses new ones until it is unlocked ("Session creation inhibited"), through the portal and through its own API alike; at GDM's sign-in screen too. There is no setting for it.
- **NuviraHub's GNOME Shell extension** `remote-access@nuvirahub.dev` (`client/service/gnome/extension.js`, module `client/service/src/gnome.rs`) makes the inhibition do nothing while it is enabled, in the session modes `user`, `unlock-dialog` and `gdm`. Mutter keeps a counter without a getter, and GNOME Shell inhibits at GDM before loading extensions, so the extension follows what GNOME Shell believes (inhibited when the mode does not allow screencasts) and gives the counter back when disabled. The idea is the one of the extension "Allow Locked Remote Desktop" (extensions.gnome.org 4338); the code is ours.
  - The user's part of unattended access (`nuvira service install`) puts it in `~/.local/share/gnome-shell/extensions` and adds it to `enabled-extensions`: lock screen, **from the next sign-in** (GNOME loads extensions when the session starts).
  - The system's part (`nuvira service system`, root) puts it in `/usr/share/gnome-shell/extensions` and enables it for the greeter, with the list the greeter had (read without the caller's own dconf database): sign-in screen, from GDM's next start. Where depends on GDM's dconf profile: with `system-db:gdm` (Fedora, upstream) in `/etc/dconf/db/gdm.d/90-nuvira-remote-access` + `dconf update`; on Debian and Ubuntu, whose profile has only a file database compiled by gdm3's `/usr/share/gdm/generate-config` at every start, in `/usr/share/gdm/dconf/95-nuvira-remote-access` (after `90-debian-settings`) + `generate-config`.
  - `metadata.json` lists GNOME Shell 45 up to the installed version and the next, written again at every installation (GNOME refuses extensions that do not list its version).
  - Removing unattended access removes both.
- **Without the extension** (not yet signed in again, or another desktop) the capture sends a dark placeholder once a second and the agent tells the viewer (`ScreenState`, `input: false`): someone must unlock it at the computer.
- **Other ways looked at** (2026-11-08): gnome-remote-desktop's system mode (RDP to GDM, but a separate headless session, not the one on the monitor); DRM/KMS capture with `uinput` input as root (works below GNOME, but tiled buffers, planes and drivers to handle). The extension keeps the same API and needed no new capture.

## Local address book

On Windows the saved passwords are protected with DPAPI. On Linux they are encrypted with AES-256-GCM, with a random key in `book.key` (mode 0600) in the data folder (`~/.config/nuvira`): it keeps them out of backups and synced folders of the address book alone, not from someone who can read the user's files.

## Tests of 2026-11-02

**Controlled Linux computer** (Ubuntu 26.04 VM, viewer on a Fedora 44 VM, both GNOME 50 on Wayland): screen at 1280×800 (openh264), mouse, keyboard with accents, wheel, clipboard with text and images both ways, files both ways (3 MB, same SHA-256); the saved permission skips the dialog; sharing resumes by itself after locking and unlocking the screen. Also from app to app through the public server, with NuviraHub's confirmation.

**Controlled Linux on X11** (2026-11-03, Mint 22.3 Cinnamon VM, from Fedora): mouse, keyboard with accents, wheel; about 25 fps while things move (software encoding 17 ms per frame), almost nothing on a still screen.

**Linux technician:** Ubuntu 26.04 (GNOME on Wayland, VM without GPU) controlling the NUC (Windows 11): direct path, 1920×1080, median latency 6 ms, decode 2.4 ms; the archive and the `.deb` from the workflow install and start, with their icons in the menu and in the dash.

## Tests of 2026-11-08 (sign-in and locked screens)

- **Mint 22.3 (LightDM, X11)**, `.deb` of the branch, viewer on PC-LABORATORIO: after logging the person out the greeter's agent started as root on `:0` with `/var/run/lightdm/root/:0`, same ID (780 221 705); greeter seen, password typed with `--type-keys` and Enter: signed in, the greeter's agent ended, the person's started by itself. Typing in a burst gave "wrong password" (the greeter's input method mixed the order): the test option now types at a person's pace. The lock screen of Cinnamon was captured by the person's agent as usual.
- **Fedora 44 / Ubuntu 26.04 (GNOME 50)**: GNOME's API opens without a dialog; closed at the lock (2.1 s after), "Session creation inhibited" while locked and at GDM's greeter (user `gdm-greeter`, uid 60578).
- **Fedora 44 (GNOME 50.0) with the extension**, agent 0.18.0, viewer on Mint: a session open before the lock stays open (no `Closed`), new ones open while locked; the viewer sees the lock screen and types into its password field. After logging out, GDM's greeter (extension active in mode `gdm`) seen from the viewer and a click on the user opened its password field. The sign-in screen's agent was first refused by SELinux (`init_t` writing to the greeter's bus socket): fixed with the `bin_t` label.
- **Ubuntu 26.04 (GNOME 50.1) with the extension**: GDM's greeter seen and clicked (mode `gdm`), lock screen seen and typed into (mode `ubuntu`, child of `user`); removal gives back gdm3's compiled settings unchanged.

## To do

- **X11, faster capture**: MIT-SHM and XDamage instead of `GetImage` and comparing images.
- **GNOME's lock screen at once:** the extension works from the next sign-in; before it the lock still closes the session.
- **Automatic updates**: signed apt and dnf repositories; the `linux-x64` channel for the archive.
- **Several screens on Wayland**: one shared screen, chosen in the system dialog.
- **Other desktops**: tested with GNOME only; KDE Plasma to be tested; Sway and Hyprland have no remote desktop portal (screen without input).
- **Performance**: DMA-BUF capture, the cursor as metadata; VA-API on Intel and AMD and NVENC on a Linux installed (both tried only through WSL).
- **Ubuntu 22.04 and Mint 21**: dropped in 0.14.0 (PipeWire too old); possible by loading PipeWire at run time.

**Unattended access** (2026-11-06): the user service on Ubuntu 26.04 and Fedora 44 (Wayland) and Mint 22.3 (X11) starts at login and is reached with the permanent password with no app open; after a restart with automatic login both came back by themselves, and on Wayland the saved permission was accepted without a dialog. The app links to the service's agent and shows its ID.
