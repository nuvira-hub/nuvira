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

Tested on 2026-11-06 with the Pro test server: support session from the queue, note, report with technician, time, minutes to invoice, CSV and print.

Tested on 2026-11-16 (Pro test server on the Mac mini, the Mac's app as the customer, PC-LABORATORIO's app as the technician):
- **Server restarted** with a request waiting (invitation for admin, 2 uses): `kept=1` at start, the Mac's agent asked again, the request back with the same time (1794817852) and "for admin", the invitation still at 1 use; taken from LAB → the Mac said "admin has taken your request" and asked to accept "the technician who took your support request" → session.
- **Network cut**: the Mac's agent frozen for 30 s (SIGSTOP: the server saw it disconnect after the QUIC timeout), then resumed: during the cut the queue empty ("put aside"), after it the request back with its time; its invitation had a single use, so a second use would have been refused.
- Unit test `TestRestartAndReconnect`: restart, order kept across returns, the invitation not spent twice, taken state after a restart, a new ticket as a new request, a device not back in time dropped.
