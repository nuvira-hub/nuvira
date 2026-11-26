# NuviraHub — Support queue and reports

[← Index](README.md)

With a NuviraHub Pro server that offers it (feature `pro.support_queue`), a customer asks for help from the app and the first free technician of the team takes the request. The apps are the same as always: the button and the queue appear only when the server announces the feature.

- **The customer.** The "Ask for support" card under the ID, with an optional message. The app then shows the place in the queue, then who took the request; the request can be cancelled.
- **The technicians.** Every account signed in to the server's address book sees the "Support queue" section: who is asking, from which computer, for how long and the message. It refreshes every 3 seconds while it is open. "Take" gives the request to the first who presses it (the server lets only one win) and opens the session.
- **Consent.** The agent makes a random ticket (128 bits) for the request and sends it to the server, which gives it to the technician who takes it. The viewer presents it inside the end-to-end encrypted session (`Auth.support_ticket`) instead of a password. The person at the PC is **always** asked to accept, with a line saying it is the technician who took their request. The server knows the ticket, but the ticket alone opens nothing. It is used once, and it ends with a cancel or when the device disconnects.
- **Server.** The base server passes the requests to a module implementing `ext.SupportDesk`; without one it answers `@server.pro_required`.
- **Network cuts and restarts** (2026-11-16). A request whose device goes away is put aside (not listed: nobody would accept) for 10 minutes. The agent keeps the request it sent and sends it again, with the same ticket, as soon as it is registered again (`support request still open: asked again after reconnecting`); the queue gives it back its place (the time it was first asked), its invitation (no other use spent) and, if taken, who took it. The queue and the invitations in use are saved in the Pro server's database (`support_requests`, `support_invited`, the tickets with them: they still open nothing without the person's consent) and read back at start, so a restart of the server loses nothing. A device that does not come back in 10 minutes leaves the queue; an app restarted makes a new ticket, so its old request lapses the same way.

Tested on 2026-11-06 with a Pro test server on a Linux VM: customer on Fedora (Wayland), technician on Linux Mint, request, take, confirmation and session.

## Reports of the support time

With the Pro session log (feature `pro.session_log`) the web console has a "Reports" page for administrators: the sessions held in a period, by customer, technician and computer, with the minutes to invoice and the amounts, exported as CSV or printed (to save as PDF).

- **Customer**: the company of the computer's entry in the address books.
- **Technician**: the account signed in to the app that opened the session. The viewer presents the app's sign-in token when it connects (`Connect.account_token`); the server only resolves it to the account name and never stores it.
- **Note**: after a session the app asks the signed-in technician what was done; the note goes to the session log (`SessionNote`, the account's latest session with that computer within a day) and into the report.
- **Billing rules** (console): rounding up to a step (default 15 minutes), a minimum per session (default 15 minutes), the currency and an hourly rate per customer.
- **Time of a session**: the agent's end-of-session report (`SessionReport`). The agent keeps it until it can give it to the server: a session that goes on while the server is unreachable (a network cut, the server restarted) is reported once the agent is registered again, and a report for a session the server no longer had in memory (it restarted) is still taken for a session of that agent with no report yet. When the report cannot come, the end is estimated and the session shows "held, end estimated" in the console (`estimated` in the CSV and in the REST API, `~` before the time):
  - the agent came back as a new process (`Register.instance` changed: it was closed, it crashed or the computer restarted): the session ended when the old process left the server (or, if the server had not noticed that yet, when the new process registered: late by the time the agent took to come back);
  - no report 48 hours after the session started: the same estimate if the agent left the server, otherwise no end and no time (not in the report);
  - sessions routed before a restart of the server and never reported: closed after 49 hours with no end and no time.

  A report that comes after an estimate replaces it.

Tested on 2026-11-06 with the Pro test server: support session from the queue, note, report with technician, time, minutes to invoice, CSV and print.

Tested on 2026-11-16 (Pro test server on the Mac mini, the Mac's app as the customer, PC-LABORATORIO's app as the technician):
- **Server restarted** with a request waiting (invitation for admin, 2 uses): `kept=1` at start, the Mac's agent asked again, the request back with the same time (1794817852) and "for admin", the invitation still at 1 use; taken from LAB → the Mac said "admin has taken your request" and asked to accept "the technician who took your support request" → session.
- **Windows** (PC-LABORATORIO's service as SYSTEM, the request from its app) and **Linux** (Mint's user service, the branch's tar.gz, the request from an invitation opened with `xdg-open`): server restarted, `kept=1`, the agent "asked again after reconnecting", the request back with its time (and on Mint "for admin" with the invitation still at 1 use); LAB's app showed position 1 again.
- **Network cut**: the Mac's agent frozen for 30 s (SIGSTOP: the server saw it disconnect after the QUIC timeout), then resumed: during the cut the queue empty ("put aside"), after it the request back with its time; its invitation had a single use, so a second use would have been refused.
- Unit test `TestRestartAndReconnect`: restart, order kept across returns, the invitation not spent twice, taken state after a restart, a new ticket as a new request, a device not back in time dropped.
