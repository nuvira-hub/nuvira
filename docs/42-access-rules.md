# NuviraHub — Rules of unattended access

[← Index](README.md)

Three rules protect the permanent passwords of a computer, set in the app
(Settings, "Unattended access", "Who can connect, and how") and saved together:

- **Computers allowed**: only the NuviraHub IDs in the list can connect with a
  permanent password; others are refused before anything else ("This computer
  does not accept connections from yours"). Empty, every computer can. With "Also
  with the temporary password" the list holds for attended sessions too (the
  temporary password, the support queue's ticket, a session handed over).
- **Two-factor code**: besides the permanent password, the 6-digit code of an
  authenticator app (TOTP, RFC 6238: 30 seconds, HMAC-SHA1, as Google
  Authenticator, Microsoft Authenticator, Aegis expect). Turned on by scanning
  the QR code shown in the app and typing a code to confirm it. Each code works
  once; after 5 wrong codes in 10 minutes the permanent passwords are refused
  until the 10 minutes have passed.
- **Access profiles**: further permanent passwords, each with what it does not
  allow (keyboard and mouse, clipboard, files, terminal, tunnels, printing,
  resolution, sound). Those are off for the whole session and the person at the
  computer cannot switch them back on; without keyboard and mouse there is no
  privacy mode and no restart either. The main permanent password stays as it is.

## Where they are

`access.json` in the folder of the agent's device key, read at every session (a
change counts at the next one):

| Installation | Folder | Who writes it |
|---|---|---|
| Windows service | `%ProgramData%\nuvira` (administrators and SYSTEM only) | `nuvira access save`, run by the app as administrator (UAC) |
| Linux, macOS, a person's unattended access | the person's data folder (`~/.config/nuvira`, `~/Library/Application Support/nuvira`) | the app |
| Linux, macOS, sign-in screen | `/var/lib/nuvira` (root's), copied with the device key and the password's hash | the system's part, after the administrator's password the app asks for when the rules change |
| Windows without the service | `%APPDATA%\nuvira` | the app |

On Windows the app cannot read the service's file: the agent sends it a view
without hashes or secret (`AgentEvent::Access`, also when the app connects and
after a save, `AgentCommand::GetAccess`), and a save sends a change
(`access::Edit`) that keeps from the file what the app does not know (the
hashes of profiles not changed, the current secret). A file that cannot be read
refuses every permanent password rather than ignoring rules someone wrote; the
app says so and saving the rules again replaces it.

From the command line, as administrator: `nuvira access show` prints the rules
(no hashes, no secret); `nuvira access save --file CHANGE.json` applies a change
and deletes the file. `--dir` picks another folder.

Installations for the whole computer (MSI with policies, `nuvira service deploy`,
the macOS package) do not show the section: rules from the organisation's
policies are to come.

## Protocol

- `Auth.code` (field 4): the two-factor code.
- `Hello.two_factor` (viewer, field 27): it asks for the code when the agent
  sends `CodeRequired`.
- `CodeRequired` (agent → viewer, instead of `AuthResult`): the password was
  right and the code is missing. The viewer asks for it and sends `Auth` again
  with the password and the code; the agent waits 3 minutes. Older viewers are
  refused with "@server.old_version" ("protocol version not supported: update
  NuviraHub"), a key they translate.
- The session window asks for the code in the box in the middle of the window
  (`notice.html`); `NUVIRA_CODE` gives it for tests. The web client closes the
  attempt, asks for the code and opens the session again with password and code.

## Tests of 2026-11-15

Agent: a test agent on PC-LABORATORIO and on the Mint VM, the installed service
of PC-LABORATORIO (this build, through alexbn.ddns.net). Viewers: Mint (Linux),
PC-LABORATORIO (Windows), and a viewer of 0.27.0.

- Allowed computers: a list without the Mint viewer refuses its permanent
  password and accepts the temporary one; with "Also with the temporary password"
  both are refused; with the Mint's ID both work. A broken `access.json`: the
  permanent password refused with its message, the agent's log says why.
- Profile "View only" (keyboard and mouse, files off): the session gets
  `input: false, files: false`, nothing listed as switched off by the person.
- Two-factor code: right code accepted; the same code again refused; the box in
  the window on Windows (typed through UI Automation) and on Linux (typed after a
  click into the field); 0.27.0's viewer told to update; after the wrong codes
  the permanent passwords are refused for a while.
- Windows service: rules saved with `nuvira access save` (the file gets the
  folder's rights: administrators and SYSTEM), then allowed list + profile + code
  together from the Mint. The app showed the rules (from the agent), a profile
  removed and "Save the rules" pressed through UI Automation, the UAC prompt
  answered from the Mint's viewer (the service's agent shows the secure desktop):
  saved, the rest kept. The code turned on again from the app (QR code and key
  shown, code confirmed): the old secret's codes refused, the new one's accepted.

Not tried yet: the copy for the sign-in screen on Linux and macOS (it needs the
administrator's password), macOS, the web client.

## Known limits

- On Linux the box for the code does not take the keyboard by itself: a click
  into it first (Windows: it does).
- A terminal-only session (`--terminal`) has no window to ask the code in: give
  it with `NUVIRA_CODE`.
