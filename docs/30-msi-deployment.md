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

Not tried yet: Intune and a real domain GPO (the template loaded in the Group Policy editor), the app's read-only server field on a PC with the policy.
