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
- **Release key**: made once with `nuvira-release keygen`, kept in `%USERPROFILE%\.nuvira\release-signing.pk8` on the machine that builds the releases, never in the repository. Its public half is built into the program (`client/update/src/lib.rs`): a tampered server, file or download cannot get anything installed, whatever the transport. Whoever has the private key can get any program installed on every device: it must be backed up offline and kept private. Losing it means no further update can be published (devices would need a program with a new public key, installed by hand).
- **Server** (`server/internal/updates`): serves only `<platform>.json` and `<platform>/nuvira-<version>.exe` from `updates` in its data, on the TCP port of the WebSocket fallback (`/nuvira/update/`). Whoever runs a server decides when its devices update by copying the files. With `NUVIRA_WS_LISTEN=off` there are no updates.
- **Client** (`client/update`): a minimal HTTP/HTTPS GET (behind a reverse proxy the URL follows `--server-ws`), at most 256 MB, written to a `.part` file, checked against size and SHA-256, then renamed.

## Installed devices (service)

The service checks 2 minutes after starting and then every 6 hours (1 hour after an error). A newer, correctly signed version is downloaded into `%ProgramData%\nuvira\update` (writable only by SYSTEM and administrators, so nobody can swap it before it runs). If a session is in progress (the agent keeps the file `busy` in the data folder while it lasts) the update waits for it to end. Then the service starts `nuvira-<version>.exe service update`, which stops the service, replaces the program in Program Files and starts the service again with the settings in place: server, ID and passwords do not change. Its output goes to `update.log`; passwords are never written there.

### When an update stops halfway

Before stopping the service the update registers the system task "NuviraHub update rescue": from 60 s later, every 5 minutes for an hour, it puts a program back if `nuvira.exe` is missing and starts the service; the service removes the task as soon as it runs. The program put back is the one the update moved aside (`nuvira.old.exe`), or else the service's own backup: 2 minutes after starting, once its agent is online, the service copies its program to `%ProgramData%\nuvira\backup\nuvira.exe` (only when it changed). The update process never touches that copy.

Why (2026-11-01): Kaspersky Anti-Ransomware Tool took the unsigned program installing the service for malware (`PDM:Trojan.Win32.Generic`), stopped it and rolled back what it had done: it deleted every file the update had written or renamed, the new program and the old one moved aside, so the service had nothing to start. With the backup the NUC was back on the previous version within a minute, with no one at the PC. The real fix is signing the program (see `tools/sign-code.sh`); this keeps a PC reachable whatever stops an update.

The Start menu shortcut is made by the program itself (no PowerShell: a script started by the update was the first thing Kaspersky flagged), only when missing, and after the service has started.

### The open app

The app open at the PC keeps running the old program (moved aside as `nuvira.old.exe`). When its link with the service's agent comes back after the update, the agent tells it its version: if newer, a bar shows "NuviraHub X has been installed" with "Restart", which starts the installed program in place of the app. It works from the update after 0.11.0 on: the open app must already have this check.

## Portable copies (app without the service)

The app checks at start-up and every 6 hours. A bar shows "NuviraHub X is available" with "Update": the new version is downloaded and checked like above, the running program is renamed aside (a running program cannot be overwritten), the new one takes its file name (so the server in `nuvira-host=....exe` stays) and is started; the old copy is deleted at the next start. With the service installed the app does not check: the service updates the installed program.

## Publishing a version

1. Raise the version in `Cargo.toml` (`[workspace.package]`) as in "Version numbers" below: devices already on it do not update.
2. `tools/publish-update.sh`: builds and signs into `dist/update/`.
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

## To do

- The server could fetch new versions from GitHub Releases by itself once the repository is public.
- Code signing of the program: Certum Open Source certificate ordered 2026-11-01; `tools/sign-code.sh` is called by `build-setup.sh` and `publish-update.sh` (before the update signature, which covers the final bytes). `NUVIRA_UNSIGNED=1` skips it. To check: whether the trailer of customised installers (appended after the signature) keeps the signature valid.
