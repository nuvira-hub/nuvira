# NuviraHub — Security model (summary)

[← Index](README.md)

- Every device generates its own key pair at first start; the private key never leaves the device (Windows DPAPI/TPM, macOS Keychain, Secret Service/TPM on Linux — for now it is a file in the user's or the service's protected data folder).
- The server only knows: ID, public key, presence, session metadata (who/when/how long). **Never** contents.
- Sessions are TLS 1.3 end-to-end between the two devices' keys (D10); the session password travels only inside that channel and is checked by the agent (D13). Planned: unattended password checked with a **PAKE** (OPAQUE/SPAKE2), so that it does not travel at all, not even encrypted.
- Attempt limits and progressive lock-out per ID (against brute force), with a warning to the remote user. Today: a 2-second pause after a wrong password and after a failed address book sign-in.
- Always-visible indicator of an active session on the remote PC, which cannot be turned off in attended mode.
- Signed automatic updates (Ed25519) distributed by your own server.
- Address book (D16): account passwords hashed with Argon2id; device passwords encrypted with AES-256-GCM using a server key kept next to the database; remembered sign-ins are random tokens stored only as SHA-256.
