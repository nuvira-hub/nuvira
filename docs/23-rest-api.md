# NuviraHub — REST API of the Pro server

[← Index](README.md)

A NuviraHub Pro server with the REST API in its licence (feature `pro.rest_api`) answers HTTP requests at `/api/v1` on its TCP port (the one of the WebSocket fallback, 21116 by default). Ticketing and management software can use it to read the devices, the address books, the session log, the support reports and the support queue, and to keep the address books up to date. The full description is in OpenAPI 3.1 at `/api/v1/openapi.json`. Anyone can read that file: it says nothing about the server.

- **Tokens.** An administrator creates them in the web console (API tab) for an account: name, read-only or read and change, expiry (30, 90 or 365 days, or never). The token (`nvh_` followed by 64 hex digits) is shown once; the server keeps only its SHA-256. The list shows the first characters, the account, the access, the expiry and the last use; a token can be revoked from there. Deleting the account deletes its tokens; changing the account's password does not.
- **Permissions.** A token acts with the role and the address books its account has at the moment of the request. Administrators see every device, book and session. Technicians and read-only accounts see the devices of their books, the sessions on those devices and their own sessions, and change only what their level allows. Reports are for administrators only. Every account sees the support queue. A read-only token may only use GET.
- **Passwords.** The passwords of the computers saved in the books can be set and changed through the API, never read: entries say only `has_password`. The support queue does not give out its tickets either.
- **Transport.** As for the console, a token sent in clear from outside the LAN is refused (`console.https_required`): over the Internet the server goes behind a reverse proxy with HTTPS that sends `X-Forwarded-Proto`. A wrong token gets a one-second pause, as a wrong password does.
- **Requests.** `Authorization: Bearer nvh_...`; JSON bodies, with unknown fields refused (so a mistyped name is an error, not a silent no-op). `PATCH` changes only the fields it carries. The times are seconds since 1970; `from` and `to` also take RFC 3339. Errors have a stable code and an English message: `{"error": {"code": "api.read_only", "message": "This API token is read only."}}`.
- **Log.** Every change made through the API is in the server's log with the account and the name of the token.

| Method and path | What |
|---|---|
| `GET /me` | The token, its account and the parts of the API the licence allows |
| `GET /devices`, `GET /devices/{id}` | Devices (filters `q`, `online`); a device with its inventory and health (light RMM) |
| `POST /devices/{id}/wake` | Wake-on-LAN through another computer of the same network |
| `GET`, `POST /books`; `GET`, `PATCH`, `DELETE /books/{id}` | Address books with their entries; shared books |
| `POST /books/{id}/entries`; `GET`, `PATCH`, `DELETE /entries/{id}` | Computers in the books |
| `GET /sessions` | Session log (filters `from`, `to`, `device`, `q`, `limit`) |
| `GET /reports` | Support time to invoice, per customer and technician |
| `GET /queue` | Support requests waiting and just taken |

Tested on 2026-11-06 with the Pro test server on a Linux VM: tokens made and revoked from the console in Edge, every call from a script on Windows, read-only and technician tokens refused where they should be, a book and an entry created, changed and deleted with the password kept and never returned; and the same calls from PowerShell on another Windows PC.

Example: the sessions held on one computer since the start of the month.

```sh
curl -H "Authorization: Bearer $NUVIRA_TOKEN" \
  "https://support.example.com/api/v1/sessions?device=123456789&from=2026-11-01T00:00:00Z"
```
