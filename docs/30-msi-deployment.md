# NuviraHub — MSI package and policies (Intune, Group Policy, RMM tools)

For organisations that install software on many PCs at once: a Windows Installer package, `nuvira.msi`, that installs NuviraHub with unattended access without asking anything, and policies to set the server and the permanent password centrally. Decisions of 2026-11-07: our own package builder (no WiX), an ADMX template for Group Policy, one signed package for everyone (the Pro customisation comes from the server through a profile code).

## The package

```text
msiexec /i nuvira.msi /qn SERVER=nuvira.example.com KEY=<fingerprint> PASSWORD=<permanent password>
msiexec /x nuvira.msi /qn
```

- **Properties**, all optional. `PROFILE`: code of a customised installer profile of a NuviraHub Pro server (see below). `SERVER`: host or host:port of the NuviraHub server; without it a first installation uses the public server, an upgrade keeps the server in place. `KEY`: fingerprint of the server's certificate (otherwise the first one seen is remembered). `PASSWORD`: permanent password for unattended access; without it the one in place is kept, or one is generated on a first installation (then only a policy, the app or a reinstallation can set it). The password never appears in Windows Installer's log (`MsiHiddenProperties`, and the command line of the action is hidden).
- **What it does.** The package is a wrapper, like the NSIS setup: it carries `nuvira.exe` and runs `nuvira.exe service install --msi-product <ProductCode> ...` as SYSTEM, the same installation as the app's "Install on this computer" and `nuvira-setup.exe /S`. Device ID and data stay in `%ProgramData%\nuvira` as always: an installation over the setup, or over the app's, keeps ID, server and password.
- **It owns no file.** The service updates the program by itself from its server; if the package owned `nuvira.exe`, a Windows Installer repair would put the older one back. Its only component is a registry value, `HKLM\SOFTWARE\nuvira\Msi\ProductCode`. A package older than the program installed (updated by the service in the meantime) leaves the newer program in place.
- **Installed apps.** One entry, the program's own, which removes NuviraHub through `MsiExec.exe /X{ProductCode}`; the package's entry is hidden (`ARPSYSTEMCOMPONENT`). `nuvira.exe service uninstall --remove-program` also hands over to Windows Installer, so that the package never stays registered with nothing installed (deployment tools would take NuviraHub for installed). The data in `%ProgramData%\nuvira` stays, with the device ID, as with the setup.
- **Versions.** Every version has its own ProductCode and the same UpgradeCode (`{4A53094B-9DEE-444F-9292-377CAAEC74C0}`): a newer package replaces the older one without stopping unattended access (the removal of the old package does not uninstall the service during an upgrade), an older package over a newer one stops with "A newer version of NuviraHub is already installed".
- **Intune.** Line-of-business app (MSI), or Win32 app with the command line above; detection by ProductCode. In both cases the properties go on the command line.
- **RMM tools** (and scripts): the same command line, as SYSTEM.
- **Group Policy** (software installation, assigned to computers): see below. Needs the package of 0.99.3 or later: the earlier ones had no advertising sequence and every assignment failed (see "Tests of 2026-11-27").

### Group Policy software installation

1. Put `nuvira.msi` on a share the computers can read (the computer accounts: `Domain Computers` or `Authenticated Users`) and use its network path, never a local one.
2. In a GPO linked to the computers' OU: Computer Configuration → Policies → Software Settings → Software installation → New → Package → the network path → **Assigned**. The settings come from the policies below (server, permanent password), since a GPO cannot pass properties.
3. Recommended, as for any software installed at startup: Computer Configuration → Administrative Templates → System → Logon → "Always wait for the network at computer startup and logon" (otherwise a PC with fast logon installs only at the second restart).
4. The PCs install it at the next restart (`gpupdate` alone only schedules it).

- **Upgrade.** Add the newer package to the same GPO, then in its Properties → Upgrades → Add… the earlier NuviraHub package with "**Package can upgrade over the existing package**" (the package removes the earlier version itself without stopping unattended access). The Group Policy editor does not link them by itself. The other choice, "Uninstall the existing package, then install", works too but stops unattended access for the time of the restart. In practice the service has often already updated the program from its server: the upgrade then only moves the package's registration to the new version.
- **Removal.** All Tasks → Remove → "Immediately uninstall the software from users and computers", for every NuviraHub package in the GPO (an older one left in place would be assigned again). At the next restart NuviraHub is removed as with `msiexec /x`; `%ProgramData%\nuvira` stays, so a later installation gets the same ID.

Building: `tools/build-msi.sh` (called by `tools/build-setup.sh`) builds `client/msi` (nuvira-msi), writes `dist/nuvira.msi` around the signed `target/release/nuvira.exe` and signs the package. The database is written through Windows Installer itself (msi.dll, as WiX does), so the builder runs only on Windows. ProductCode and package code are derived from the version and the program, not random: the same program and version give the same package. The MSI is about the size of `nuvira.exe` (32 MB): the program is stored as it is, not compressed.

## Policies (Group Policy, Intune)

A Group Policy software installation cannot pass properties to a package: the settings come from policies instead, which also apply to PCs already installed. The template is in `deploy/windows/policy`: `nuvira.admx` and the texts in five languages (`en-US`, `it-IT`, `de-DE`, `fr-FR`, `es-ES`). Copy them into the central store (`\\domain\SYSVOL\domain\Policies\PolicyDefinitions`) or into `C:\Windows\PolicyDefinitions`; in Intune, import the ADMX (Devices → Configuration → Import ADMX). They appear under Computer Configuration → Administrative Templates → NuviraHub.

| Policy | Value under `HKLM\SOFTWARE\Policies\nuvira` | Effect |
|---|---|---|
| Server | `Server`, `ServerKey` | Server (and fingerprint) of the service and of the app; in the app the field is read-only, with "The server is set by your organisation's policies". |
| Permanent password | `PasswordHash` | Permanent password of unattended access. Only its hash: every user of the PC can read the policies. `nuvira.exe service password-hash` makes it (it asks for the password); use a long random one, since whoever reads the hash can try passwords against it offline. |
| Customised installer profile (Pro) | `Profile` | Code of a customised installer profile of the Pro server: wins over the `PROFILE` of the installation. |

The service applies the policies when it starts and checks them every minute: when they change it ends and the service manager starts it again within 5 seconds (its failure actions), with the new settings saved in `service.json`. Removing a policy does not bring the earlier value back: the PC keeps the last one.

## Customised installers through the package (NuviraHub Pro)

One package, signed, for every customer: the customisation does not go into the MSI (the server cannot sign it, and the customised `.exe` keeps its own way, docs/18). Each profile of the web console (Installers) has a code, made by the server and never changed; the console shows with each profile the command line ready to copy, `msiexec /i nuvira.msi /qn SERVER=<server> KEY=<fingerprint> PROFILE=<code>`.

- The service of a PC installed with `PROFILE=` (or given the policy) asks its server for the profile's customisation (`GetBranding` on the signalling connection, with the certificate checked against `KEY` or the remembered fingerprint) when it starts and once a day, and keeps it next to the program (`%ProgramFiles%\nuvira\branding.json`), where the app reads it as for a customised program: name, logo, colour, support contacts, preset and locked server and language. A changed profile reaches the PCs within a day, the app shows it from its next start.
- The request needs no sign-in: the code is the key (12 characters, 60 random bits). The answer is what the customised program would carry, nothing more; on a server without the CUSTOM_INSTALLER entitlement it is `@server.pro_required`.
- A profile belongs to its server: installing on another server drops it.

## Tests of 2026-11-07

PC-LABORATORIO (Windows 10, the service installed earlier with the app's installation), unsigned development build:
- `msiexec /i nuvira.msi /qn` without properties over that installation: same ID, server and password; the installed apps entry moved to `MsiExec.exe /X{...}`.
- Upgrade 0.16.1 → 0.16.2: the old package removed, the service never uninstalled; 0.16.1 over 0.16.2 refused ("A newer version…", 1603).
- `msiexec /x`: service, program folder, Start menu shortcut, installed apps entry and package registration gone, `%ProgramData%\nuvira` kept; installed again with `SERVER=… PASSWORD=…`: same ID, the password works from the viewer, the password is not in the log (searched as UTF-16 and ANSI).
- `nuvira.exe service uninstall --remove-program` with the package installed: handed over to `msiexec /x`, nothing left registered; a later installation without the package writes the program's own uninstall command again.
- Policies written into the registry as Group Policy does: within a minute the service started again with the server and the password of the policies; the viewer got in with the new password, not with the old one.
- The package signed with the code signing certificate (Authenticode on the MSI, `Valid`) still opens in Windows Installer.

- Profile: on the Pro test server (Ubuntu VM) a profile "Rossi Assistenza" (colour, support text and phone, locked server, pinned fingerprint); the console showed the command line; `msiexec /i … SERVER=… KEY=… PROFILE=<code>` on PC-LABORATORIO: within a second the service saved the customisation, the app started in the user's session showed name, colour and support box. Removed with `msiexec /x`, the program folder went with the customisation; a later installation on the earlier server dropped the profile.

- App with the Server policy (PC-LABORATORIO, user session): Settings showed the policy's server in a field that cannot be changed (field and Save dimmed, also the server of a customised installer) with "The server is set by your organisation's policies"; a reinstallation with another `--server` ended up on the policy's server as soon as the service started.

## Tests of 2026-11-27 (real domain, Group Policy)

A test domain `nuvira.test` on two evaluation VMs: a domain controller with Windows Server 2025 Standard (Desktop Experience, 26100.32230) and a client with Windows 11 Enterprise 26H2 (26300.9457) joined to it, in its own OU. A GPO linked to that OU only; the policies set with the ADMX template: server and fingerprint of the NAS Pro development server, the hash of a test password, "Always wait for the network". Packages on a share of the controller.

- **ADMX.** `nuvira.admx` and the five `.adml` (as in v0.99.2) copied into the central store with Windows' own templates: the editor says "Policy definitions (ADMX files) retrieved from the central store", shows NuviraHub with Server and Permanent password as Enabled and the Pro profile as Not configured, the Server dialog reads back host and fingerprint, help text in English.
- **The released packages failed.** `nuvira.msi` of v0.99.1 (as published, signed) added to the GPO without complaint, but at every startup the client logged "The assignment of application NuviraHub … failed. The error was: 1603" (events 101/108 of Application Management Group Policy), with no Windows Installer log at all, also with verbose logging on. The cause: the package had no `AdvtExecuteSequence` table. Group Policy installs by advertising the package first (from the script the editor makes when the package is added), and with no sequence there is nothing to advertise; `msiexec /jm` on the package also "succeeded" (0) without advertising anything (a later `msiexec /x` answered 1605). `msiexec /i`, Intune and RMM tools never use that sequence, which is why the tests of 2026-11-07 passed. All the packages up to 0.99.2 have the gap.
- **Fix** (nuvira-msi): an `AdvtExecuteSequence` with CostInitialize, CostFinalize, InstallValidate, InstallInitialize, PublishFeatures, PublishProduct, InstallFinalize. Test packages built with the fixed builder around the signed `nuvira.exe` of v0.99.1 and v0.99.2 (same ProductCodes as the released packages; unsigned, Group Policy does not need a signature). `msiexec /jm` now advertises it (product state 1, then 0 = removed with `/x`).
- **Installation.** The fixed 0.99.1 package assigned in place of the released one (removed from the GPO): at the next restart "The assignment of application NuviraHub … succeeded" (301) and "The install … succeeded" (302) within 40 s; service running, `nuvira.exe` 0.99.1, `service.json` with the server, fingerprint and password hash of the policies and the package's ProductCode, registered on the policy's server (ID 447 327 309); one visible entry in installed apps ("NuviraHub 0.99.1", `MsiExec.exe /X{…}`), the package's own hidden. From the web client of that server: a wrong password refused ("access denied: wrong password"), the policy's password opened a session on the client (lock screen through the service).
- **Self-update.** Six minutes later the service updated the program to 0.99.2 from its server by itself ("installing version 0.99.2 (attempt 1)"), with the package still at 0.99.1, as designed.
- **Upgrade through Group Policy.** The fixed 0.99.2 package added to the GPO (Advanced); the editor did not link it to the earlier one: Upgrades → Add… → NuviraHub, "Package can upgrade over the existing package". At the next restart: "NuviraHub 0.99.2 … is an upgrade of application NuviraHub" (202), the earlier assignment removed (303), 0.99.2 assigned and installed (301, 302); service running, program 0.99.2, ProductCode of 0.99.2 in the registry and in `service.json`, one entry "NuviraHub 0.99.2", the same ID; the password still opened a session. Since the program was already 0.99.2 by then, this saw the package's registration move, not the package replacing an older program.
- **Change of a policy on an installed PC.** The password hash in the GPO changed to a new password, `gpupdate /target:computer` on the client: within a minute the service logged "the organisation's policies for NuviraHub changed: starting again with them" and saved the new hash; from the web client the old password was refused and the new one opened a session.
- **Removal through Group Policy.** Both packages removed from the GPO with "Immediately uninstall": at the next restart the assignment removed (303) and "Windows Installer removed the product … 0.99.2 … status 0"; service, program folder, `HKLM\SOFTWARE\nuvira\Msi`, installed apps entries gone, both ProductCodes unknown to Windows Installer; `%ProgramData%\nuvira` kept (device key, settings, logs).
- **Installation again.** The 0.99.2 package assigned again: installed at the next restart (301, 302), registered with the same ID 447 327 309.

Seen on the way: the editor shows the package's Publisher empty (the package has `Manufacturer`): cosmetic, not looked into. The software installation runs only at startup; a `gpupdate /force` while it is pending asks "OK to restart? (Y/N)".

- **The released 0.99.3 package** (signed, from the release build, same bytes as the GitHub file), added to the same GPO as an upgrade of the 0.99.2 test package ("upgrade over"). This time the NAS had no 0.99.3 update yet, so the service could not update itself first: at the next restart 202/303/301/302, and the package replaced the program, `nuvira.exe` 0.99.2 → 0.99.3 (same hash as the release, signature valid), ProductCode of 0.99.3, one entry "NuviraHub 0.99.3", the same ID; the policy's password opened a session from the web client.

Not tried: Intune; "Uninstall the existing package, then install" as the upgrade choice; Windows 10 and Windows Server as GPO clients; a PC that is off when the package is removed.
