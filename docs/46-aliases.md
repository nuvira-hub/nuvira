# NuviraHub — Aliases: a name instead of the ID

[← Index](README.md)

A computer can have a readable name on its server, such as `office-mario`, that
whoever connects types instead of the 9-digit ID.

- **Rules**: 3 to 32 of `a-z`, `0-9` and `-`, at least one letter (so it is
  never taken for an ID), no `-` at the ends; unique on the server whatever the
  case (`Office-Mario` is `office-mario`). Empty: no alias.
- **Setting it**: the app, Settings → "This computer" → "Save the name"
  (`set_alias` → `AgentCommand::SetAlias` → `SetAlias` to the server). The
  server answers `AliasState { alias, error }`: the alias it keeps now, and why
  the one asked for is not it (`@server.alias_taken`, `@server.alias_invalid`,
  `@server.alias_failed`); a refused one leaves the earlier alias in place. The
  app shows the alias under the ID, and the error under the field.
- **Kept by the agent**: `alias.txt` next to the device key holds the alias the
  server confirmed and is sent at every registration (`Register.alias`), so a
  server that lost its database gets it back. A taken one stays in the file
  (asked for again at the next start); one that is not valid gives way to the
  server's. Removing the alias deletes the file.
- **Older agents** send no `Register.alias` (an `optional` field): the server
  keeps the alias it has. Only reachable devices have one (`api_only` never).
- **Server**: `devices.alias` (added to earlier databases at start, with a
  unique index on non-empty aliases); `Connect.device_id` is an ID (spaces,
  dashes and dots ignored) or an alias, resolved by `Store.ResolveTarget`; an
  unknown alias is "the device is not reachable".
- **Where an alias is accepted**: the viewer (`nuvira-viewer --server S
  office-mario`), the app's "Control a computer" form, the local address book
  (kept as typed) and its CSV import (`id` column), the web client (NuviraHub
  Pro), the Pro address book (the console, the app, the REST API: the alias
  becomes the computer's ID, so that the book shows it online;
  `@err.alias_unknown` when no computer has it).

## Tests of 2026-11-15

Test server on the Mint VM, first the build of the previous commit (database
of that version), then this one on the same database.

| Test | Result |
|---|---|
| Database of the previous version opened by this server | `alias` column and the unique index added, the device registered before kept |
| Agent A (NUC) with `alias.txt` = `Ufficio-Prova` | the server keeps `ufficio-prova` |
| Agent B (NUC) asking the same alias | refused, `@server.alias_taken` |
| PC-LABORATORIO's viewer, `" Ufficio-Prova "` (spaces, capitals) | connected to agent A, 29.6 fps |
| Viewer, an alias no computer has | "the device is not reachable" |
| Mint app: a taken name, through Enter | the message under the field, no alias |
| Mint app: `Mint-Laboratorio` with the button | `mint-laboratorio`, shown under the ID |
| Mint app: `caffè-1` | "Not a valid name…", `mint-laboratorio` kept |
| Agent restarted with a refused name in `alias.txt` (left by an earlier build) | the server's alias written back to the file |
| Mint app's form: `Ufficio-Prova` and the password | session with agent A; the local address book saved `ufficio-prova` |
| PC-LENOVO-ALE's viewer (Windows, MX150) → `Mint-Laboratorio` | connected |
| Mint app: name emptied | the alias removed, `alias.txt` deleted, the old alias no longer reachable |
| Agent B takes the freed name; agent A restarted | B has it; A keeps `ufficio-prova`; `MINT-laboratorio` reaches B |
| NuviraHub 0.27.0 (no aliases) with agent A's key | registered, alias kept, reachable by `ufficio-prova` |
| Pro test server (VM Ubuntu): its database of earlier versions (28 devices) | migrated at start |
| Web client (Firefox on Mint) → `Ufficio-Prova` | session with agent A |
| Pro console API: entry with device `Ufficio-Prova` | saved with agent A's ID; `nessuno-cosi` → `@err.alias_unknown` |

Unit tests: `store.TestAliases` (old schema, taken, the invalid ones, resolving,
freed, reopened), `accounts.TestEntryByAlias`, `signal::tests::targets`.

Not tried yet: the app and agent on macOS (same code: next rental); Chrome and
Safari for the web client; the Pro address book from the app's form and from the
REST API (the same `PutEntry` as the console, tried through it); the CSV import
with an alias (unit test of the parser only, `valid_target`); setting the alias
from the Windows app (its code is the Linux one: Windows was tried as agent with
`alias.txt` and as viewer).

Not done yet: `name@server` to name the server with the alias; aliases set and
locked by the admins in the Pro console; the alias in the console's list of
devices.
