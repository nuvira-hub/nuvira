# NuviraHub — Automatic updates

[← Index](README.md)

Choices made with the user (2026-10-27): updates come from the device's own nuvira-server; installed devices update by themselves, portable copies offer the update; every version is signed with a NuviraHub release key.

## How it works

```
release machine                   nuvira-server                   devices
nuvira-release sign  ──copy──►  data/updates/windows-x64.json   ◄── GET (every 6 h)
                                data/updates/windows-x64/
                                  nuvira-0.0.2.exe              ◄── GET, checked
```

- **Manifest** (`windows-x64.json`): version, platform, size, SHA-256 of the program and an Ed25519 signature over all of them.
- **Channels**: `windows-x64` (`nuvira-<v>.exe`), `linux-x64` (`nuvira-<v>`, the program alone), `linux-x64-deb` and `linux-x64-rpm` (`nuvira-<v>.deb`/`.rpm`), `macos` (`nuvira-<v>.zip`, the signed NuviraHub.app). The server serves only these names; servers older than 0.17.0 serve only the Windows one.
- **Release key**: made once with `nuvira-release keygen`, kept in `%USERPROFILE%\.nuvira\release-signing.pk8` on the machine that builds the releases, never in the repository. Its public half is built into the program (`client/update/src/lib.rs`): a tampered server, file or download cannot get anything installed, whatever the transport. Whoever has the private key can get any program installed on every device: it must be backed up offline and kept private. The program also trusts a **backup key** (since 0.21.1), made on 2026-11-09 and kept offline, never on the build machine: if the key in use is lost or stolen, the next release is signed with the backup (`nuvira-release sign --key <backup>`) and brings a new pair in `PUBLIC_KEYS`; the installed copies update as usual. Without the backup too, devices would need a program with new public keys, installed by hand.
- **Server** (`server/internal/updates`): serves only `<platform>.json` and `<platform>/nuvira-<version>.exe` from `updates` in its data, on the TCP port of the WebSocket fallback (`/nuvira/update/`). Whoever runs a server decides when its devices update by copying the files. With `NUVIRA_WS_LISTEN=off` there are no updates.
- **Client** (`client/update`): a minimal HTTP/HTTPS GET (behind a reverse proxy the URL follows `--server-ws`), at most 256 MB, written to a `.part` file, checked against size and SHA-256, then renamed.

## Installed devices (service)

The service checks 2 minutes after starting and then every 6 hours (1 hour after an error). A newer, correctly signed version is downloaded into `%ProgramData%\nuvira\update` (writable only by SYSTEM and administrators, so nobody can swap it before it runs), always as `nuvira-update.exe`: a file already there is used again only if its size and SHA-256 are the manifest's. The fixed name is for antiviruses that trust single files by path (Kaspersky Anti-Ransomware Tool): `C:\Program Files\nuvira\nuvira.exe`, `C:\ProgramData\nuvira\backup\nuvira.exe` and `C:\ProgramData\nuvira\update\nuvira-update.exe` are excluded once for all versions. Until 0.21.1 the file was `nuvira-<version>.exe`. If a session is in progress (the agent keeps the file `busy` in the data folder while it lasts) the update waits for it to end. Then the service starts `nuvira-update.exe service update`, which stops the service, replaces the program in Program Files and starts the service again with the settings in place: server, ID and passwords do not change. Its output goes to `update.log`; passwords are never written there.

### When an update stops halfway

Before stopping the service the update registers the system task "NuviraHub update rescue": from 60 s later, every 5 minutes for an hour, it puts a program back if `nuvira.exe` is missing and starts the service; the service removes the task as soon as it runs. The program put back is the one the update moved aside (`nuvira.old.exe`), or else the service's own backup: 2 minutes after starting, once its agent is online, the service copies its program to `%ProgramData%\nuvira\backup\nuvira.exe` (only when it changed). The update process never touches that copy.

Why (2026-11-01): Kaspersky Anti-Ransomware Tool took the unsigned program installing the service for malware (`PDM:Trojan.Win32.Generic`), stopped it and rolled back what it had done: it deleted every file the update had written or renamed, the new program and the old one moved aside, so the service had nothing to start. With the backup the NUC was back on the previous version within a minute, with no one at the PC. The real fix is signing the program (see `tools/sign-code.sh`); this keeps a PC reachable whatever stops an update.

### When something keeps stopping it (from 0.22.4)

An antivirus or anti-ransomware program can stop every update without anything showing on the PC: on the NUC Kaspersky Anti-Ransomware Tool refused each one for days, and only `service.log` said so ("Access denied", every hour). Now the service counts its installations per version in `%ProgramData%\nuvira\update-attempts` ("0.22.4 2"). Starting the same version a second time means the first did not go through, so it writes that version to `update-blocked` in the same folder. The service's agent reads that file when an app connects and every minute after, and sends it in its `Info` message (`update_blocked`; older apps ignore the field). The app shows a notice with no button: a security program on the PC is probably stopping the update, allow the three files above in it, or install the new version by hand from nuvirahub.dev. Once the service finds no newer version (the update went through, or the person installed it), it removes both files, and the notice goes away within a minute. Tried on 2026-11-11 on PC-LABORATORIO with a test build: the file written by hand, the notice shown in the open app, then gone a minute after the service's check removed the file.

The Start menu shortcut is made by the program itself (no PowerShell: a script started by the update was the first thing Kaspersky flagged), only when missing, and after the service has started.

### The open app

The app open at the PC keeps running the old program (moved aside as `nuvira.old.exe`). When its link with the service's agent comes back after the update, the agent tells it its version: if newer, a bar shows "NuviraHub X has been installed" with "Restart", which starts the installed program in place of the app. It works from the update after 0.11.0 on: the open app must already have this check.

## Linux and macOS (2026-11-07)

There is no separate service process: the user service (systemd user unit on Linux, LaunchAgent on macOS) runs `nuvira agent --ipc`, and that agent updates (`client/update/src/unix.rs`). What it replaces depends on how NuviraHub was installed:

| Installed | Who updates | Channel | How |
|---|---|---|---|
| Linux, `install.sh` of the tar.gz (`~/.local/share/nuvira`) or any copy the user can write | the user's agent, or the app without unattended access | `linux-x64` | the program is copied beside the old one, started once (`agent --version`), then renamed into place; the old one is kept as `nuvira.old` until the next start |
| Linux, .deb or .rpm (`/usr/bin/nuvira`) | root's `nuvira-update.timer` of the package | `linux-x64-deb` / `-rpm` | `nuvira service update-package`: `apt-get install` / `dnf install` of the downloaded package |
| macOS, NuviraHub.app in a folder the user can write (/Applications for an administrator) | the user's agent, or the app without unattended access | `macos` | the zip is unpacked with `ditto` beside the app (`.NuviraHub-update`), its signature checked (`codesign --verify --deep --strict`, same designated requirement as the running app: macOS keeps the Screen Recording and Accessibility permissions only for that), started once, then the bundles are swapped |
| macOS for the whole Mac (the `.pkg`, an MDM): NuviraHub.app is root's | the installation's root service (`nuvira service managed-run`), since 0.22.2 | `macos` | as above, run by root, from the installation's server, 2 minutes after the service starts and then every hour, not during a session; the new app is made root's again (`chown -R root:wheel`), then the virtual keyboard daemon is started again and the service ends: launchd starts the new program and the person's agent follows |
| macOS for a standard user, a program in a folder of root's | nobody (logged once by the agent) | — | updated by hand |

- **The agent** (user service only) checks 2 minutes after starting and every 6 hours (1 hour after an error). During a session it keeps `busy` in the data folder (`~/.config/nuvira`, `~/Library/Application Support/nuvira`): an update found then is downloaded and installed 5 minutes after the session ends. Every 15 s it looks whether its program file was replaced (by itself, by root's timer or by hand): if so and no session is in progress it ends, and systemd (`Restart=always`) or launchd (`KeepAlive`) starts the new program.
- **Root's timer** (`/usr/lib/systemd/system/nuvira-update.{timer,service}`, enabled by the package's postinstall, disabled when it is removed): 5 minutes after boot and then every hour. The server is the `--server` of a user's unattended access (`~/.config/systemd/user/nuvira-agent.service` of the users with uid ≥ 1000), otherwise NuviraHub's public server. Downloads go to `/var/cache/nuvira` (root only). If a user's `busy` file exists it waits for the next hour. Output in the journal (`journalctl -u nuvira-update`).
- **The open app** behaves as on Windows: with unattended access the bar "NuviraHub X has been installed" with "Restart" appears when the agent comes back with a newer version; without it, "NuviraHub X is available" with "Update", if this user can replace the program. "Restart" starts the program at the path it was started from (on Linux `current_exe` would follow the file to its old copy).
- **Not updated**: the CUPS backend of the NuviraHub printer in `/usr/lib/cups/backend` (root's), except by the packages, whose postinstall copies it again. Its protocol carries a version (`NUVIRA-PRINT 1`).

## Portable copies (app without the service)

The app checks at start-up and every 6 hours. A bar shows "NuviraHub X is available" with "Update": the new version is downloaded and checked like above, the running program is renamed aside (a running program cannot be overwritten), the new one takes its file name (so the server in `nuvira-host=....exe` stays) and is started; the old copy is deleted at the next start. With the service installed the app does not check: the service updates the installed program.

## Publishing a version

1. Raise the version in `Cargo.toml` (`[workspace.package]`) as in "Version numbers" below: devices already on it do not update.
2. `tools/publish-update.sh`: builds and signs into `dist/update/`.
   For Linux and macOS: download the artifacts of the CI runs of the same commit (`gh run download … -D dist/ci`) and run `tools/sign-unix-updates.sh dist/ci`: it signs `linux-x64`, `linux-x64-deb`, `linux-x64-rpm` and `macos` into `dist/update/` and makes `dist/nuvira-update-VERSION.zip` again with every channel.
3. Copy the contents of `dist/update/` into `updates` in the server's data (on Unraid `/mnt/user/appdata/nuvira-server/updates`).

## Version numbers

One version for the whole workspace (`Cargo.toml`), raised when a release is published, not at every change; each release is tagged `vX.Y.Z` (the tag also builds the server image with that version).

| Change since the last release | Before 1.0 | From 1.0 |
|---|---|---|
| Fixes only | patch: 0.2.**1** | patch: 1.2.**1** |
| New features, compatible | minor: 0.**3**.0 | minor: 1.**3**.0 |
| Breaks compatibility between app, agent and server (protocol, data) | minor: 0.**3**.0 | major: **2**.0.0 |

1.0.0 is the milestone of the roadmap (end of phase 5), decided with the project owner.

## Tests of 2026-10-27

- Unit tests: signature (other key or any field changed: rejected), versions, URLs, download from a local HTTP server (wrong hash or size: rejected, nothing left). Server: only the expected names, no way out of the folder, no POST, manifest not cached.
- PC-LABORATORIO with 0.0.1 and the server on the NUC publishing 0.0.2: "version 0.0.2 available" 2 minutes after start, downloaded and checked in 4 s, service stopped, program replaced and service started again in 2 s, same ID and password.
- Portable 0.0.1 on the NUC: the bar appeared, "Update" replaced the program (same SHA-256 as 0.0.2, same file name) and restarted it.
- Not yet tested on a real device: waiting for a session in progress to end.

## Tests of 2026-11-07 (Linux)

nuvira-server of this branch on the Ubuntu VM (port 21316), test version 0.16.0.1 signed with the release key (any real release is newer; the files were deleted after the test).
- Mint, agent `--ipc` run from `~/nuvira-upd` by a systemd user unit with `Restart=always` (as the user service): 2 minutes after start "version 0.16.0.1 available", downloaded and checked in 0.5 s, tried and swapped (new inode); 15 s later the agent ended, systemd started the new program, back online with the same ID; `nuvira.old` gone at that start.
- The same with a session open from the Fedora viewer: downloaded, "session in progress: the update waits for it to end"; installed at the next try 5 minutes later, after the session.
- Root's timer with the .deb on Mint: enabled and started by the postinstall; the server taken from the user's unattended access; with a `busy` file downloaded into `/var/cache/nuvira` and "session in progress: next time"; without, `apt-get` installed 0.16.0.1 (a repackaged .deb), the timer stayed active, the NuviraHub printer stayed, the download was removed.
- Unit tests: channels and file names, program replaced only if the new one starts (a broken one leaves the old in place), bundle path, /etc/passwd and unit parsing; server: the new names served, others refused.

## Test of 2026-11-09 (backup release key)

The update path signed only with the backup key, end to end. nuvira-server on the Ubuntu VM (port 21316), Mint's agent: the published 0.21.1 Linux program, run with `--ipc` from a folder of the user by the systemd user unit, pointed at that server.
- Manifest with one byte of the signature changed: "update check failed: the update signature is not valid", nothing downloaded.
- The same program as 0.21.1.1, signed with `nuvira-release sign --key <backup>`: 2 minutes after start "version 0.21.1.1 available", downloaded and checked in 0.3 s, installed (new inode); 15 s later the agent ended, systemd started the new program, same ID.
- `nuvira-release sign` with a key not built into the program refuses to sign.
The test files and the unit change were removed afterwards; the backup key was only read on the NUC, not copied.

## Test of 2026-11-09 (fixed download name)

PC-LABORATORIO's service with main (f6448b5, built on the NUC), pointed at a nuvira-server on PC-LABORATORIO itself (127.0.0.1:21196) publishing the same program as 0.21.1.3, signed with the release key. The update folder still held `nuvira-0.21.1.exe` of the previous update.
- 2 minutes after start: "version 0.21.1.3 available", downloaded and checked in 4 s as `update\nuvira-update.exe`, `nuvira-0.21.1.exe` removed, installed, service started again.
- The program still says 0.21.1, so 2 minutes later it found 0.21.1.3 again: no download (same size and SHA-256), installed from the file already there; again 2 minutes later.
- The file replaced with 18 bytes of text: at the next check downloaded again (33.8 MB) and installed.
Afterwards the manifest was removed, the service put back on alexbn.ddns.net (ID 260 905 122 as before) and the test server and files removed.

Not tested yet: macOS (the bundle swap, signature and designated requirement check, the Screen Recording and Accessibility permissions after the swap, a standard user), the .rpm with dnf, the app's "Update" and "Restart" bars on Linux and macOS, an agent of a package install restarting after root's update.

## Test of 2026-11-10 (Mac installed with the .pkg)

Mac mini M2 Pro, NuviraHub 0.22.0 from the `.pkg` (app of root's), server the NAS publishing 0.22.1. The person's agent: "version 0.22.1 available", downloaded and checked, then "moving the old app aside: Permission denied (os error 13)": a folder moved into another needs write access to the folder itself, and the app is root's. So Macs deployed by the package or an MDM never updated by themselves. (The earlier macOS test, 2026-11-07, had the app dragged from the dmg, the person's.)

Fixed in main (0d56212, 0b3d0e5): the agent sees an app of root's (`RootBundle`) and leaves it to the root service, which updates it. Tried with the CI's signed and notarized package of main installed by `sudo installer`, a test nuvira-server on the Mac (127.0.0.1:21316) publishing that app as 0.22.1.1 and then 0.22.1.2 signed with the release key, the organisation's profile pointing at it:
- 2 minutes after the service started: available, downloaded and checked in 0.1 s, installed in 1.1 s, "the app was updated: starting again with the new one"; the service, the virtual keyboard daemon and the person's agent came back with the new program, same ID on the server;
- the first try left the app owned by the archive's user (`m1:staff`): `chown -R root:wheel` added; the second try kept it `root:wheel`;
- `codesign --verify --deep --strict` valid and Gatekeeper "Notarized Developer ID" after the swap.
Afterwards the profile and the server were put back (NAS) and the test files removed.

## To do

- The server could fetch new versions from GitHub Releases by itself once the repository is public.
- Code signing of the program: Certum Open Source Code Signing in the cloud, issued 2026-11-02 to "Open Source Developer Alessandro Blason" (valid until 2027-11-02, RSA 3072). SimplySign Desktop must be connected on the build machine (code from the SimplySign app on the owner's phone); the certificate is then in the user's store and `tools/sign-code.sh` signs with Certum's timestamp (about 5 s per file, no prompts). It is called by `build-setup.sh` and `publish-update.sh` (before the update signature, which covers the final bytes). `NUVIRA_UNSIGNED=1` skips it.
- Customised installers (Pro): the profile appended after the signature made Windows take the program for unsigned. The Pro generator now puts it inside the certificate table, as padding of the signature (both lengths updated, the signed bytes unchanged); the file still ends with the magic, so the app reads it as before. Verified with a program signed by Certum: signature valid for `Get-AuthenticodeSignature` and `signtool verify /pa`. Only Windows' optional strict check (`EnableCertPaddingCheck`, off by default) would refuse it.
