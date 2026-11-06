# NuviraHub — Support queue

[← Index](README.md)

With a NuviraHub Pro server that offers it (feature `pro.support_queue`), a customer asks for help from the app and the first free technician of the team takes the request. The apps are the same as always: the button and the queue appear only when the server announces the feature.

- **The customer.** The "Ask for support" card under the ID, with an optional message. The app then shows the place in the queue, then who took the request; the request can be cancelled.
- **The technicians.** Every account signed in to the server's address book sees the "Support queue" section: who is asking, from which computer, for how long and the message. It refreshes every 3 seconds while it is open. "Take" gives the request to the first who presses it (the server lets only one win) and opens the session.
- **Consent.** The agent makes a random ticket (128 bits) for the request and sends it to the server, which gives it to the technician who takes it. The viewer presents it inside the end-to-end encrypted session (`Auth.support_ticket`) instead of a password. The person at the PC is **always** asked to accept, with a line saying it is the technician who took their request. The server knows the ticket, but the ticket alone opens nothing. It is used once, and it ends with a cancel or when the device disconnects.
- **Server.** The base server passes the requests to a module implementing `ext.SupportDesk`; without one it answers `@server.pro_required`. The queue lives in the Pro server's memory: after a restart customers ask again.

Tested on 2026-11-06 with a Pro test server on a Linux VM: customer on Fedora (Wayland), technician on Linux Mint, request, take, confirmation and session.
