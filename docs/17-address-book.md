# NexoDesk — Central address book

[← Index](README.md)

Part of phase 2 (decision D16). Like Supremo or RustDesk Pro, the address book is kept **on the server**: a technician signs in from the NexoDesk app with an account and finds their devices on any PC.

## Choices made with the user

| Topic | Choice |
|---|---|
| Accounts | Several technicians, each with their own address books |
| Device passwords | Saved, encrypted on the server (D17) |
| Account and book management | Only from the NexoDesk app (no web console before phase 5) |
| Adding devices | Automatically at the first successful connection, plus by hand |

## Model (`server/internal/accounts`)

| Table | Content |
|---|---|
| `accounts` | username (case-insensitive), Argon2id hash of the password, administrator flag |
| `tokens` | SHA-256 of the sign-in tokens, with expiry (30 days) |
| `books` | address books: `owner_id` set = personal book of that account ("Personal", created with the account, one per account, not shareable); `owner_id` NULL = shared book |
| `book_members` | accounts that see a shared book |
| `entries` | device ID (9 digits), name, company, contact, e-mail, phone, notes, encrypted password; one entry per device per book |

Foreign keys are enforced (`PRAGMA foreign_keys`): deleting an account deletes its tokens, its personal book and its entries.

Visibility: an account sees its personal book and the shared books it is a member of; an administrator also sees every shared book and manages accounts and shared books. An administrator cannot remove their own rights or delete their own account.

## Security

- **Account passwords**: Argon2id (2 passes, 19 MB, OWASP parameters), at least 8 characters; a 1 s pause after a failed sign-in.
- **Tokens**: random, returned only at sign-in, stored on the server as SHA-256. The app saves the token in `app.json`, never the password. Logout deletes the token; changing the password invalidates all of the account's tokens.
- **Device passwords**: AES-256-GCM with the key `address-book.key` in the server's data folder (created at first start, readable only by the owner). The database alone is not enough to read them; the key must be backed up with it.
- **Transport**: the API runs inside the QUIC/TLS connection to the server, whose certificate the app pins at the first connection (as the agent and viewer do).
- **Errors** are translation keys (`@server.denied`, `@server.password_too_short`, …) that the app shows in the user's language (D15).
- **First administrator**: `NEXODESK_ADMIN_USER` and `NEXODESK_ADMIN_PASSWORD` when the server starts with no accounts; without them an `admin` account is created with a random password written once to the server log, to be changed at the first sign-in.

## API (`proto/nexodesk/v1/rendezvous.proto`)

The app opens a signalling connection with `Register.api_only` (it does not appear as a device) and sends `ApiRequest` messages, each answered by an `ApiResponse` with a result or an error.

| Request | Who | Effect |
|---|---|---|
| `Login`, `LoginToken`, `Logout` | everyone | sign in with password or saved token, sign out |
| `ChangePassword` | everyone | own password, the old one is required |
| `GetBooks` | everyone | visible books with their entries (passwords decrypted) |
| `PutEntry`, `DeleteEntry` | everyone, in visible books | create/update or delete an entry |
| `AutoAdd` | everyone | adds the device to the personal book if it is in no visible book |
| `ListAccounts`, `PutAccount`, `DeleteAccount` | administrators | account management |
| `PutBook`, `DeleteBook` | administrators | shared books and their members |

## App (`client/app/src/address_book.rs`, `ui/address-book.js`)

- Address book section of the icon rail (D18): sign-in panel, then the list of books and entries with a search field; double click or "Connect" starts the session with the saved password.
- Entry form: all fields, password included (the password field stays masked).
- Administration section (administrators only): accounts (create, set the password, administrator flag, delete) and shared books with their members.
- **Automatic add**: when the viewer reports `@connected` on its standard output, the app calls `AutoAdd` with the ID, the remote device's name and the password used.
- If the token has expired or was revoked the app goes back to the sign-in panel; if the connection to the server dropped it reopens it with the saved token.

## Tests

- `server/internal/accounts/accounts_test.go`: sign-in, tokens, personal and shared books, visibility between accounts, administrator rights, encryption of the passwords, automatic add.
- Test of 2026-10-27 between the NUC and PC-LABORATORIO: sign-in, automatic add at the first connection, connection from the address book with the saved password, shared book seen by a second account, account management. Tested by the user.

## Rules for all the devices (policy)

Administration → "Rules for all the computers" (phase 5, 2026-10-27): the person at the computer must always accept (also with the permanent password), no file transfers, no shared clipboard. Stored by the server (`policy` table), sent to each device at registration (`Registered.policy`) and to every connected device as soon as an administrator changes them (`ServerMessage.policy`); applied by the agent at the start of each session. They can only restrict: a device never allows more than its own settings because of them. With "always accept" and no app open on the PC (service only), sessions are refused.

Tested on the NUC test server: rules received at registration; with the permanent password the viewer showed "waiting for the person to accept", then "no answer" after 30 s.

## To do

- Groups or folders inside a book, online status of the devices in the list.
- Two-factor authentication and sign-in policies (phase 5).
- Import from RustDesk/Supremo address books.
