# NuviraHub — Linux

[← Index](README.md)

Since 0.13.0 the app runs on Linux and controls other computers: ID, address book, connecting, the session window with its toolbar. The Linux computer itself cannot be controlled yet (no agent, no service): the app says so and hides that part of the main window (`CONTROLLABLE` in `client/app/src/main.rs`). Automatic updates exist only for Windows for now: on Linux the app does not check.

## Packages

`.github/workflows/client-linux.yml` builds them on Ubuntu 22.04 with `tools/package-linux.sh`, at every change to the client and at every tag. The binary needs a glibc at least as new as the one it was built with: building on the oldest distribution we support keeps it running on the newer ones (Ubuntu 22.04+, Debian 12+, Mint 21+, Fedora 40+). The packages are a workflow artifact; they go into the GitHub release by hand, with the signed Windows files.

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

## Local address book

On Windows the saved passwords are protected with DPAPI. On Linux they are encrypted with AES-256-GCM, with a random key in `book.key` (mode 0600) in the data folder (`~/.config/nuvira`): it keeps them out of backups and synced folders of the address book alone, not from someone who can read the user's files.

## Tests of 2026-11-02

Ubuntu 26.04 (GNOME on Wayland, VM without GPU) controlling the NUC (Windows 11): direct path, 1920×1080, median latency 6 ms, decode 2.4 ms; the archive and the `.deb` from the workflow install and start, with their icons in the menu and in the dash.
