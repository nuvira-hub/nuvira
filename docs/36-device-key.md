# Device key protected by the system

[← Index](README.md)

The device key (Ed25519) is what the 9-digit ID stands for: the server binds the public key to the ID, and every session's TLS certificate is signed with it. Whoever copies the private key can take the computer's ID. Since 0.21 the key files are kept protected by the system wherever it can, not only by the permissions of their folder.

Code: `client/transport/src/keystore.rs` (formats and the systems' stores), `identity.rs` (load, migration, hand-over). Every key goes through it: `device.key` (the agent's ID, the person's and the service's), `viewer.key`, `app.key`, `profile.key`.

## How each system keeps it

| Who | Windows | macOS | Linux |
|---|---|---|---|
| A person (app, viewer, their own unattended access) | DPAPI of the person's account | AES-256-GCM, wrapping key in their **login keychain** (unlocked at sign-in, automatic login included) | AES-256-GCM, wrapping key in the **Secret Service** (GNOME Keyring, KWallet), only when its collection is unlocked and the computer does not sign in automatically; otherwise the file (0600) |
| The system (service, sign-in screen, installation for the whole computer) | DPAPI of SYSTEM (`%ProgramData%\nuvira`) | wrapping key in the **System keychain** (root) | **systemd-creds** (root): TPM 2.0 when present, bound to no PCR, plus the host key in `/var/lib/systemd` |

- **TPM on Windows.** Ed25519 is not a TPM 2.0 algorithm, so the key itself cannot live in the TPM; DPAPI's master keys, protected by Windows with the account's credentials (SYSTEM's for the service), are what keeps it.
- **No PCRs with systemd-creds.** A firmware or Secure Boot update changes PCR 7; a key bound to it would no longer open, and the computer would lose its ID.
- **Why the Secret Service is used with care.** With automatic sign-in the keyring stays locked until someone types the password (seen on Ubuntu 26.04: `login` collection `Locked true`), so after a remote restart the agent could not start and the computer would not come back. With automatic sign-in on (GDM, LightDM, SDDM) the key stays a file; if automatic sign-in is turned on later, the key goes back to the file at the next start.
- The About page of the app tells which protection holds the key of this computer's ID.

## File format

A key file starts with `NVK1` and one letter: `D` DPAPI, `K` Keychain, `S` Secret Service, `C` systemd-creds, `F` file permissions only, `H` handed over (below). A file of PKCS#8 DER only (starting with `0x30`) is a key written before 0.21.

What is written is read back before it replaces the file, so a store that takes the key but cannot give it back does not lose the ID.

## Migration

At the first load by 0.21 a plain key is protected in place, keeping the ID (the same key, only stored differently). If the system cannot protect it now (macOS over SSH, no keyring), it stays as it is and is tried again at the next load.

**Going back to 0.20 or older does not work with a protected key**: those versions read only plain files. To go back, set `NUVIRA_KEY_STORE=file` and start 0.21 once, or keep a copy of the old key file.

## When the key cannot be opened

- **Not now** (keyring locked, login keychain not reachable, as over SSH on a Mac): the program waits and tries again every 5 s; for the Secret Service it asks the person to unlock it (the system's prompt).
- **Never here** (another computer or account, the wrapping key deleted, DPAPI keys lost after an administrator reset the account's password): the file is kept as `*.unreadable` and a new key is made, hence a **new ID**. The error is in the log.
- **Another account's file** (root opening a person's key or the other way round, on Linux and macOS): an error, nothing is replaced.

## Handing the key to another account

Some parts of NuviraHub run as another account with the same ID:

- **Sign-in screen** (Linux and macOS): the person's key goes to root's folder. Root cannot open the person's keychain or Secret Service, so before the administrator prompt the app writes a handed-over copy, `device.key.handed`, next to the person's key; root imports it with its own protection and removes it (the app removes it too, also when the prompt is cancelled).
- **Installation for the whole computer** (Linux and macOS): root hands its key to the agent of the person in front of the computer as an `H` file in `/run/nuvira-session-<uid>` (`/var/run` on macOS), that person's only and gone at the next restart, as before; the greeter's agent on Linux gets one in `/run/nuvira-greeter-<uid>`. Root compares the copies by public key.

An `H` file is used as it is and never protected in place: the account that handed it checks it. A session copy written before 0.21 (plain) is not taken as up to date: the system service hands it again as an `H` file when it starts, before the person's agent can protect it for itself (seen on the Mac mini: then root can no longer compare it, and hands it again at its next start).

## macOS Keychain notes

- The wrapping key's item (service `io.github.nuvira-hub.nuvira`, account `device-keys`) opens without a prompt for the program that made it, which for NuviraHub is the signed `nuvira` of the app (updates keep the same signature). An item made by another program (an unsigned build, `key-probe`) is refused to the app: the key then stays a file. Delete it with `security delete-generic-password -s io.github.nuvira-hub.nuvira -a device-keys [/Library/Keychains/System.keychain]`.
- Over SSH the login keychain is not reachable (`errSecInteractionNotAllowed`): a person's protected key waits there; a plain one stays plain. Root's System keychain works everywhere, the sign-in screen included.

## Tests

- `cargo test -p nuvira-transport` (format, DPAPI round trip on Windows, migration, unreadable keys).
- `cargo run -p nuvira-transport --example key-probe -- PATH [--hand-over TO | --import FROM]` loads a key and prints how it is kept; `NUVIRA_KEY_STORE=<name>` forces a protection.

Live, 2026-11-09:

| Machine | Done |
|---|---|
| PC-LABORATORIO, Windows 11, service | `%ProgramData%\nuvira\device.key` (plain, from 0.20.1) became `NVK1D` at the service's first start; ID 260 905 122 kept, registered on the server, a viewer's direct QUIC/TLS path to it opened |
| NUC, Windows 11, a person | `key-probe`: DPAPI, read back, hand-over |
| Mac mini M4, macOS 27.2, signed app (CI run 42578828038) | `nuvira-macos.pkg` over the managed installation: after removing the item `key-probe` had made, root's key → `NVK1K` (System keychain), ID 362 492 018 kept; the session agent protected the old plain copy in the login keychain and registered with the same ID; the system service handed an `H` copy at its next start, same ID |
| Mac mini M4, `key-probe` | person in the GUI session: plain → login keychain, same key; over SSH: plain stays plain, a protected one waits; root: System keychain; hand-over → import by root; root's key opened by the person: error, nothing replaced |
| Ubuntu 26.04 VM | automatic sign-in on, `login` collection locked: the file is kept; isolated GNOME Keyring (private bus): file → Secret Service with the same key, read back, keyring replaced → `*.unreadable` and a new key |
| Fedora 44 VM, root | systemd-creds (host key, no TPM in the VM): file → `NVK1C`, same key, hand-over to the person |
| Fedora 44 VM, installation for the whole computer | release build in /usr/bin: `service deploy` → `/var/lib/nuvira/device.key` `NVK1C`; session copy `NVK1H`, agent registered with ID 341 019 207; no further hand-over in 3 minutes; GDM greeter: copy `NVK1H` in `/run/nuvira-greeter-60578`, the greeter's agent registered with the same ID; back to the session (GDM restarted, automatic sign-in), same ID |

Not tried live yet: the sign-in screen's installation from a person's own install (the copy `device.key.handed` through the administrator prompt), Secret Service on a real computer without automatic sign-in, KWallet, a computer with a TPM (Windows has nothing to try there; systemd-creds with TPM 2.0 on Linux).
