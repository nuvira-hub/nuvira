# NuviraHub — Linux installation for the whole computer (Ansible, Puppet, Salt, scripts)

For organisations that install software on many Linux computers at once: the `.deb` or `.rpm` package plus one command as root, or a policy file put there before the package, and the computer can be reached with a permanent password from then on, with nobody signing in first. It is the Linux side of the Windows MSI package and policies ([30-msi-deployment.md](30-msi-deployment.md)).

## The command

```text
sudo nuvira service deploy [--server HOST[:PORT]] [--server-key FINGERPRINT] [--password-hash HASH | --password-file FILE] [--profile CODE]
sudo nuvira service deploy --remove
nuvira service password-hash
```

- **Options**, all optional, as the MSI properties. `--server`: the NuviraHub server; without it a first installation uses the public server and a later one keeps the server in place. `--server-key`: its certificate fingerprint (otherwise each person's agent remembers the first one it sees: pass it). `--password-hash`: the permanent password as a hash, from `nuvira service password-hash` (it asks for the password); `--password-file`: the password in clear text in a file, deleted after reading. Without either the password in place is kept, or one is generated and printed once. `--profile`: code of a customised installer profile of a NuviraHub Pro server.
- **Again** (or from the package's update) it keeps what it is not given: ID, password, server.
- **Over a person's own installation** (the app's "Install on this computer" with the administrator password, which copied that person's ID into `/var/lib/nuvira`): the computer keeps that ID, and that person's own installation gives way at their next sign-in.
- **Remove**: `deploy --remove`, or removing the package. The computer keeps its ID, password and server in `/var/lib/nuvira` for a later installation, as Windows keeps `%ProgramData%\nuvira`: delete the folder to forget them. The policy file stays.

## What is installed

Unlike the person's own installation (a systemd user service, docs/20), the ID belongs to the computer:

| What | Where | For |
|---|---|---|
| Device key, password hash, settings | `/var/lib/nuvira` (root only) | the computer's ID |
| System service `nuvira-signin.service` | `/etc/systemd/system` | the sign-in screen, and handing the ID to the person at the computer |
| User unit `nuvira-agent.service` | `/etc/systemd/user` | the agent in each person's session, as that person |
| Autostart entry | `/etc/xdg/autostart/nuvira-agent.desktop` | at sign-in, gives the session's display and D-Bus to systemd |
| State for the app | `/etc/nuvira/managed.json` (readable by all, no password) | the app shows "Installed by your organisation", the server fixed |
| Pro customisation | `/etc/nuvira/branding.json` | name, logo, colours, contacts of the profile |
| GNOME extension, printer | as with the person's installation | lock and sign-in screens, remote printing |

**One agent at a time.** The system service follows who is at the computer (the active session of `seat0`, from logind):
- sign-in screen: its agent as root (X11), as GDM's user (Wayland) or as SDDM's user through KWin (Wayland), as before;
- a person's graphical session: a copy of the key, the server and the hash in `/run/nuvira-session-<uid>` (that person's only, in memory), then `nuvira service start` as that person, in their systemd: the agent runs as them, so it sees their screen, their files, and opens their terminal;
- another session becomes active (switch user, sign out): that agent stops and its copy is removed.

On GNOME the agent uses GNOME's own remote desktop API, which asks nothing: no click at the computer is needed, also after the first installation. On other Wayland desktops (KDE) the portal asks the person at the computer once. On GNOME the lock screen works at once (the extension is in the system's folder before the person signs in).

What a person signed in at the computer can read is their copy of the key and of the hash, while they are signed in (their agent runs as them), as with their own installation; the hash is the same as in the policy file, so use a long random password.

## Policies

The settings can come from a file instead, also for computers already installed: `/etc/nuvira/policy.json`, root's and writable by root only (otherwise ignored):

```json
{
  "server": "nuvira.example.com",
  "server_key": "ab12…",
  "password_hash": "$argon2id$…",
  "profile": "K7Q2M9X4P1RT"
}
```

All keys optional. They win over the command's options and the settings in place. The system service reads them when it starts and checks them every minute: when they change it starts again (systemd, within 5 seconds) with the new settings, and the person's agent with it. Removing a key does not bring the earlier value back (as on Windows). The app reads the same file: the server field is fixed, with "set by your organisation's policies".

**With the package.** If `/etc/nuvira/policy.json` is there when the package is installed, the package's script runs `nuvira service deploy` by itself: with Ansible, copy the file, then install the package. Every update of the package runs it again (the installation follows the new program).

```yaml
- name: NuviraHub policy
  ansible.builtin.copy:
    dest: /etc/nuvira/policy.json
    content: '{"server": "nuvira.example.com", "server_key": "{{ nuvira_key }}", "password_hash": "{{ nuvira_hash }}"}'
    owner: root
    mode: "0644"
- name: NuviraHub
  ansible.builtin.apt:
    deb: https://nuvira.example.com/download/nuvira_amd64.deb
```

## The app on such a computer

Settings → Unattended access shows "Installed by your organisation for the whole computer…", with no Install or Remove; the server cannot be changed. The ID and the temporary password are the service's, as with the person's own installation.

## Reaching a person's systemd

The system service talks to each person's systemd as that person, with their runtime folder (`/run/user/<uid>`: `systemctl --user`, `systemd-run --user`). `systemctl --user --machine=<user>@.host`, which goes through systemd-machined, failed on Fedora 44 ("Transport endpoint is not connected"): the earlier stop of the person's agent at the sign-in screen used it and failed there without a word; it now goes the same way.

## Tests of 2026-11-08

Fedora 44 (GNOME, Wayland), alessandro's own installation in place (user service + system part, server alexbn.ddns.net, ID 341 019 207); the program of the branch in `/usr/bin/nuvira`, viewer on the NUC through the server:
- `sudo nuvira service deploy` with no options: server and ID kept, alessandro's agent stopped; the system service handed the copy to his session (`/run/nuvira-session-1000`, his, 0700) and started the unit from `/etc/systemd/user`; his own unit and autostart entry removed by `service start`. Registered again as 341 019 207.
- `/etc/nuvira/policy.json` with a new password hash: within the minute "the organisation's policies changed", the service started again, the agent with it; the viewer got in with the new password (GNOME's API, no dialog at the computer).
- Signed out: the copy removed and that agent stopped, the GDM agent took the same ID; GDM seen from the viewer. Signed in again (automatic login): the copy handed over, GDM's agent ended.
- Locked: the extension, enabled at sign-in from the system's folder, kept the session open; the lock screen seen and woken from the viewer.
- App: connected to the service's agent with the computer's ID; Settings showed "Installed by your organisation…", the server fixed, no Install or Remove.
- The packages' scripts run by hand (`preremove.sh 0`, `postinstall.sh 1`, no package built): removed everything but `/var/lib/nuvira` and the policy file; with the policy file there the script installed again by itself, same server and ID.

Not tried: Ubuntu and Mint (sudo needs the password there: X11/LightDM and Ubuntu's GDM profile), a real `.deb`/`.rpm` built by the CI, two people switching user, KDE (portal dialog), a Pro profile.

## Not yet

- Macs: the same through a `.pkg` and a `.mobileconfig` profile (see nuvira-pro/docs/internal/30-platform-parity.md).
- The device key in `/var/lib/nuvira` is protected with systemd-creds (TPM 2.0 when present, plus the host key); the copies handed to the sessions are plain files of that person only ([36](36-device-key.md)).
