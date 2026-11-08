# NuviraHub — macOS installation for the whole Mac (MDM: Jamf, Intune, Kandji, Mosyle)

For organisations that manage their Macs with an MDM: `nuvira-macos.pkg` and a configuration profile, and the Mac can be reached with a permanent password from then on, with the Mac's own ID. It is the Mac side of the Windows MSI package and policies ([30-msi-deployment.md](30-msi-deployment.md)) and of the Linux installation for the whole computer ([34-linux-deployment.md](34-linux-deployment.md)).

## The package

`nuvira-macos.pkg` (built by `tools/package-macos.sh` next to the `.dmg`) puts NuviraHub.app into /Applications and runs, as root:

```text
/Applications/NuviraHub.app/Contents/MacOS/nuvira service deploy
```

The same command can be run by hand or by a script, with the options of Linux: `--server`, `--server-key`, `--password-hash` (from `nuvira service password-hash`) or `--password-file`, `--profile`; `deploy --remove` removes it. Without options a first installation uses the public server and generates a password (printed once, in the installer's log); later ones keep what is in place.

- **Signed and notarized.** MDMs want a package signed with a *Developer ID Installer* certificate (not the *Application* one of the app). The CI signs and notarizes it when the certificate is in the secrets `MACOS_INSTALLER_P12` and `MACOS_INSTALLER_PASSWORD`; without them the package is built unsigned (`sudo installer -pkg nuvira-macos.pkg -target /` takes it, an MDM does not).
- **Never older over newer.** The automatic updates replace the app in /Applications; the package does not put an older one over it (`BundleIsVersionChecked`), and its script then runs `deploy` with the app in place.
- **A failure stops the installation**, so that the MDM reports it.

## What is installed

| What | Where | For |
|---|---|---|
| Device key, password hash, settings | `/Library/Application Support/NuviraHub` (root only) | the Mac's ID |
| LaunchDaemon `io.github.nuvira-hub.nuvira.managed` | `/Library/LaunchDaemons` | follows the person in front of the Mac, the profile, the Pro customisation |
| LaunchAgent `io.github.nuvira-hub.nuvira.managed-agent` | `/Library/LaunchAgents` | the agent in each person's desktop session, as that person |
| Sign-in screen's agent, virtual keyboard, printer | as with the person's own installation ([33-macos.md](33-macos.md)) | sign-in screen, lock screen, remote printing |
| State for the app | `/Library/Application Support/io.github.nuvira-hub.nuvira/managed.json` (readable by all, no password) | "Installed by your organisation", server fixed |
| Pro customisation | the same folder, `branding.json` | name, logo, colours, contacts of the profile |

**One agent at a time.** The daemon follows the console user (`SCDynamicStoreCopyConsoleUser`: the person in front, also with fast user switching). To that person it hands a copy of the key, the server and the hash in `/var/run/nuvira-session-<uid>` (theirs only, gone at the next restart) and starts the system's LaunchAgent in their session (`launchctl kickstart`, or `bootstrap gui/<uid>` when it was installed while they were signed in). When another person comes to the front, that copy is removed and the agent stopped (without the copy it ends at once and launchd leaves it). At the sign-in screen the sign-in screen's agent has the same ID.

**Permissions.** The agent is the program inside the app's bundle, so the Screen Recording and Accessibility permissions, which macOS keeps for the whole Mac, hold for every person. A profile can allow Accessibility, not Screen Recording (Apple): someone at the Mac allows it once in System Settings → Privacy & Security → Screen & System Audio Recording; with the profile below a standard user may do it.

**A person's own installation** (Install in the app) gives way: at the first start of the system's agent in their session their LaunchAgent is unloaded and removed (same ID, it would take it back).

## The configuration profile

`deploy/macos/nuvira.mobileconfig` is an example with four payloads, to be adapted (values, new UUIDs) and sent by the MDM:

1. **NuviraHub's settings**, managed preferences of `io.github.nuvira-hub.nuvira`: `Server`, `ServerKey`, `PasswordHash`, `Profile` (the keys of the Windows policies). In Jamf: Application & Custom Settings; in Intune: Preference file. They arrive in `/Library/Managed Preferences/io.github.nuvira-hub.nuvira.plist`; without an MDM an administrator can write the same keys with `sudo defaults write /Library/Preferences/io.github.nuvira-hub.nuvira Server nuvira.example.com` (the MDM's win). Only files of root's count.
2. **Privacy (PPPC)**: Accessibility allowed for the app (its code requirement, team 2T2G3ULLTF); Screen Recording left to a standard user.
3. **System extension**: Karabiner's driver extension for the virtual keyboard (team G43BCU2T37) allowed beforehand: the agent asks macOS to load it at its first start and it loads with no click.
4. **Background items** (`com.apple.servicemanagement`): NuviraHub's and the driver's items cannot be switched off in Login Items.

The daemon reads the settings at start and every minute: when they change it starts again (launchd, within 5 seconds) with them, and the person's agent with it. Removing a key does not bring the earlier value back.

## The app on such a Mac

Settings → Unattended access shows "Installed by your organisation for the whole computer…", with no Install or Remove; the server cannot be changed.

## Limits

- After a restart the Mac shows the sign-in screen (reachable); FileVault's unlock before macOS starts is not.
- The lock screen of a signed-in person: as with the person's own installation, no picture, typing through the virtual keyboard ([33-macos.md](33-macos.md)).

## Tests of 2026-11-08 on the Mac mini (macOS 27.2)

Package of the CI (unsigned: no Installer certificate yet), NuviraHub 0.17.0 in /Applications before, viewer on the NUC through alexbn.ddns.net:
- Settings without an MDM: `sudo defaults write /Library/Preferences/io.github.nuvira-hub.nuvira Server alexbn.ddns.net` and `PasswordHash`; `sudo installer -pkg nuvira-macos.pkg -target /`: installed, the daemon handed the copy to m1 (`/var/run/nuvira-session-501`), the system's LaunchAgent loaded in his session (installed while he was signed in: `bootstrap gui/501`), registered as 362 492 018; the viewer got in with the password of the preferences and saw the desktop (the Screen Recording permission of the app held). The app, started again, showed the Mac's ID and the server.
- Without the profile macOS asked to allow Karabiner's driver extension (expected: the profile's system extension payload allows it beforehand; not tried).
- A new `PasswordHash`: within the minute "the organisation's profile changed", started again by launchd, the agent with it; in with the new password.
- `deploy --remove`: daemon, agents' jobs, copy and state file gone, `/Library/Application Support/NuviraHub` kept; the package again: same ID.
- m1's own installation (`nuvira service install`, ID 660 850 769) and then the package: his LaunchAgent unloaded and removed, one agent left with the Mac's ID.

Not tried: an MDM and the profile (PPPC, system extension, background items), a signed and notarized package, the sign-in screen and fast user switching (they need the accounts' passwords at the Mac), the Pro profile.
