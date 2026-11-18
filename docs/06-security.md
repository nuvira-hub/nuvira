# NuviraHub — Security model (summary)

[← Index](README.md)

- Every device generates its own key pair at first start; the private key never leaves the device and is kept protected by the system: Windows DPAPI, the macOS Keychain, the Secret Service or systemd-creds (TPM 2.0 when present) on Linux, the file's permissions only where the system offers nothing better ([36](36-device-key.md)).
- The server only knows: ID, public key, presence, session metadata (who/when/how long). **Never** contents.
- Sessions are TLS 1.3 end-to-end between the two devices' keys (D10); the session password travels only inside that channel and is checked by the agent (D13). Planned: unattended password checked with a **PAKE** (OPAQUE/SPAKE2), so that it does not travel at all, not even encrypted.
- Attempt limits and progressive lock-out per ID (against brute force), with a warning to the remote user. Today: a 2-second pause after a wrong password and after a failed address book sign-in.
- Before the viewer has authenticated, the agent waits at most 10 s for each step of the handshake (the control stream in LAN mode and switch sides, the viewer's `Hello`, then its password; the confirmation by the person at the PC, 30 s, and the two-factor code, 3 minutes, keep their own waits), accepts messages of at most 64 KiB on the control stream (16 MiB after authentication; 64 KiB on the input stream), and writes at most 64 characters of the viewer's device name, system and version to its log. A connection that has not authenticated does not make the computer busy, so knowing the ID is not enough to keep other viewers out: at most 8 sessions through the server may wait for their password at once (more are told the computer is busy), and of two viewers connecting together the first whose credentials are right gets the session, the other is told the computer is busy (review lot 2: AGT-1, AGT-2, AGT-3, NET-3).
- Always-visible indicator of an active session on the remote PC, which cannot be turned off in attended mode.
- Signed automatic updates (Ed25519) distributed by your own server.
- Address book (D16): account passwords hashed with Argon2id; device passwords encrypted with AES-256-GCM using a server key kept next to the database; remembered sign-ins are random tokens stored only as SHA-256.
