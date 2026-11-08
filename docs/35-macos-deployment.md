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

1. **NuviraHub's settings**, managed preferences of `io.github.nuvira-hub.nuvira`: `Server`, `ServerKey`, `PasswordHash`, `Profile` (the keys of the Windows policies) and `Keyboard`: `iso`, `ansi` or `auto` (the default, what macOS reports). A Mac without a keyboard of its own (Mac mini, Mac Studio) reports ANSI, so with an ISO layout (Italian, German, French…) the key left of 1 and the one left of Z come out swapped: set `iso` there. It wins over `NUVIRA_MAC_KEYBOARD` for every agent of the Mac (the app's, a person's own installation, the managed one, the sign-in screen's), read when the agent starts; in the managed installation a change starts the person's agent again, elsewhere it counts from the agent's next start. The agent's log says which: `Mac keyboard type iso=… forced=…`. In Jamf: Application & Custom Settings; in Intune: Preference file. They arrive in `/Library/Managed Preferences/io.github.nuvira-hub.nuvira.plist`; without an MDM an administrator can write the same keys with `sudo defaults write /Library/Preferences/io.github.nuvira-hub.nuvira Server nuvira.example.com` (the MDM's win). Only files of root's count.
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

Later the same day, viewer on PC-LABORATORIO, a second (standard) account `nvtest` made with `sysadminctl`:
- m1 signed out: the daemon removed his copy and stopped his agent ("the person is no longer in front of the Mac"); the sign-in screen's agent took the Mac's ID and the viewer saw the sign-in screen with the managed password, and clicked the list of accounts open.
- Automatic login of `nvtest` and a restart: the daemon handed the ID to uid 503 and the system's LaunchAgent registered as 362 492 018 in that session; the viewer saw nvtest's desktop at once (the Mac-wide Screen Recording permission held, no Setup Assistant). macOS showed that person once "nuvira is requesting to bypass the system private window picker" (Allow / Open System Settings): a click from the viewer on Allow closed it, and input worked as nvtest (the Accessibility permission held as well). So a new person at a managed Mac can be helped from the start, the first session begins with that prompt.
- Back to m1's automatic login: ID handed to uid 501 again.
- Keyboard from the Windows viewer of the NUC (Italian keyboard), typed by a person into TextEdit: with the Mac on "U.S." the accented keys gave `[;'\§±` (by position, as expected); on "Italian – Pro" `èòàù` right, but `<`, `>` and `\` swapped (the Mac mini has no keyboard, so macOS reports ANSI); with `NUVIRA_MAC_KEYBOARD=iso` added by hand to the system's LaunchAgent all right. The left Alt of the PC is Option: Alt+è / Alt++ gave `[` `]`, Alt+Shift+è / Alt+Shift++ `{` `}` (as "Italian – Pro" has them); the Windows key is Cmd (Cmd+click selected several files in the Finder). AltGr+è typed nothing: Windows sends AltGr as left Ctrl + right Alt, so the Mac got Ctrl+Option; the viewer on Windows now lets that Ctrl go towards a Mac (`AltGrFix` in client/viewer/src/lib.rs) and AltGr gives what Option gives. The managed installation now has the `Keyboard` key in the managed preferences for it.
- With nvtest the agent's encoder test chose OpenH264 (5.0 ms against 8.6 ms for VideoToolbox from RAM): a choice, not an error.

Release 0.20.1, the signed package of the CI (Developer ID Installer, notarized: `spctl --assess --type install` "Notarized Developer ID"), with `sudo installer` over the managed installation: upgraded, ID and settings kept. It showed that the agent of the person in front, ending to start again with the new program, exited with 0, which the LaunchAgent (`SuccessfulExit` false) does not restart: the Mac stayed unreachable until the next sign-in. Fixed in 0.20.1 (exit code 3); checked by replacing the program under a running 0.20.1 agent: "restarting with the new one", exit 3, started again by launchd in 2 s with the same ID. An agent of 0.20.0 or older still ends for good at that first upgrade (0.20.0 was not published).

The `Keyboard` key (branch mac-keyboard-policy, package of the CI over 0.20.1), with `sudo defaults write /Library/Preferences/io.github.nuvira-hub.nuvira Keyboard …`: `iso` gave the agent of the person in front `NUVIRA_MAC_KEYBOARD=iso` (also in their `agent.json`); deleting the key, then `ansi`, then `iso` again each started the agent again within about a minute with no variable, `ansi`, `iso`. Mind for tests: `launchctl kickstart -k` does not read the job's property list again, so a variable added by hand stays in the loaded job until `bootout` and `bootstrap` (or signing out). Then (package of the CI with the key read at the program's start) an agent started by hand in m1's session, without the variable, with the Mint viewer moving the mouse: `Mac keyboard type iso=true forced="iso"` with the key, `iso=false forced="no"` without it. The sign-in screen's agent with the key was not tried (it reads it at every start, as the variables of `signin.env`).

Not tried: an MDM and the profile (PPPC, system extension, background items), typing a password at the sign-in screen of the managed installation (the same agent as in [33-macos.md](33-macos.md), where it worked; here the viewer's `--type-keys` stalled after a few letters on a nearly still sign-in screen and was not followed up), fast user switching with both people signed in, the Pro profile.
