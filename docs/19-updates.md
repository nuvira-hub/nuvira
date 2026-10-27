# NexoDesk — Automatic updates

[← Index](README.md)

Choices made with the user (2026-10-27): updates come from the device's own nexodesk-server; installed devices update by themselves, portable copies offer the update; every version is signed with a NexoDesk release key.

## How it works

```
release machine                     nexodesk-server                   devices
nexodesk-release sign  ──copy──►  data/updates/windows-x64.json   ◄── GET (every 6 h)
                                  data/updates/windows-x64/
                                    nexodesk-0.0.2.exe             ◄── GET, checked
```

- **Manifest** (`windows-x64.json`): version, platform, size, SHA-256 of the program and an Ed25519 signature over all of them.
- **Release key**: made once with `nexodesk-release keygen`, kept in `%USERPROFILE%\.nexodesk\release-signing.pk8` on the machine that builds the releases, never in the repository. Its public half is built into the program (`client/update/src/lib.rs`): a tampered server, file or download cannot get anything installed, whatever the transport. Whoever has the private key can get any program installed on every device: it must be backed up offline and kept private. Losing it means no further update can be published (devices would need a program with a new public key, installed by hand).
- **Server** (`server/internal/updates`): serves only `<platform>.json` and `<platform>/nexodesk-<version>.exe` from `updates` in its data, on the TCP port of the WebSocket fallback (`/nexodesk/update/`). Whoever runs a server decides when its devices update by copying the files. With `NEXODESK_WS_LISTEN=off` there are no updates.
- **Client** (`client/update`): a minimal HTTP/HTTPS GET (behind a reverse proxy the URL follows `--server-ws`), at most 256 MB, written to a `.part` file, checked against size and SHA-256, then renamed.

## Installed devices (service)

The service checks 2 minutes after starting and then every 6 hours (1 hour after an error). A newer, correctly signed version is downloaded into `%ProgramData%\NexoDesk\update` (writable only by SYSTEM and administrators, so nobody can swap it before it runs). If a session is in progress (the agent keeps the file `busy` in the data folder while it lasts) the update waits for it to end. Then the service starts `nexodesk-<version>.exe service update`, which stops the service, replaces the program in Program Files and starts the service again with the settings in place: server, ID and passwords do not change. Its output goes to `update.log`; passwords are never written there.

## Portable copies (app without the service)

The app checks at start-up and every 6 hours. A bar shows "NexoDesk X is available" with "Update": the new version is downloaded and checked like above, the running program is renamed aside (a running program cannot be overwritten), the new one takes its file name (so the server in `nexodesk-host=....exe` stays) and is started; the old copy is deleted at the next start. With the service installed the app does not check: the service updates the installed program.

## Publishing a version

1. Raise the version in `Cargo.toml` (`[workspace.package]`): devices already on it do not update.
2. `tools/publish-update.sh`: builds and signs into `dist/update/`.
3. Copy the contents of `dist/update/` into `updates` in the server's data (on Unraid `/mnt/user/appdata/nexodesk-server/updates`).

## Tests of 2026-10-27

- Unit tests: signature (other key or any field changed: rejected), versions, URLs, download from a local HTTP server (wrong hash or size: rejected, nothing left). Server: only the expected names, no way out of the folder, no POST, manifest not cached.
- PC-LABORATORIO with 0.0.1 and the server on the NUC publishing 0.0.2: "version 0.0.2 available" 2 minutes after start, downloaded and checked in 4 s, service stopped, program replaced and service started again in 2 s, same ID and password.
- Portable 0.0.1 on the NUC: the bar appeared, "Update" replaced the program (same SHA-256 as 0.0.2, same file name) and restarted it.
- Not yet tested on a real device: waiting for a session in progress to end.

## To do

- The server could fetch new versions from GitHub Releases by itself once the repository is public.
- Code signing of the program (planned, [08-roadmap.md](08-roadmap.md)): independent of the update signature.
