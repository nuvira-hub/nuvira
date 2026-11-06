# NuviraHub — TCP tunnels

[← Index](README.md)

With a NuviraHub Pro server that offers it (entitlement `TUNNEL`, feature `pro.tunnel`), a port of the technician's computer can lead, through the session, to an address of the controlled computer's local network: the web panel of a router or NAS, the Remote Desktop of that PC or of another PC of the same network. Decisions of 2026-11-06: Pro; only local destinations.

- **From the viewer.** The toolbar button with two arrows opens "Tunnels": address in the remote network, port and local port (empty: any free one); shortcuts for the Remote Desktop and the web panel of the PC itself. A tunnel ready shows `localhost:PORT → host:port` with "Open in the browser" and, on Windows, "Remote Desktop" (`mstsc /v:localhost:PORT`). Closing the window keeps the tunnels; they end with the session.
- **Command line.** `nuvira viewer --tunnel LOCAL:HOST:PORT` (repeatable, LOCAL 0 = any free port), also with `--terminal` for a session without desktop; each tunnel ready prints `@tunnel<TAB>local port<TAB>host:port`.
- **Local port.** Only on 127.0.0.1 of the technician's computer: other computers of the technician's network cannot use it.

## Destinations

The agent connects only to addresses of its local networks: loopback (127.0.0.0/8, ::1), private (10/8, 172.16/12, 192.168/16, IPv6 unique local fc00::/7) and link-local (169.254/16, fe80::/10). A name is resolved on the controlled computer and must give one of them. Anything else (internet, CGNAT 100.64/10) is refused with "only addresses of the remote local network can be reached": the PC must not become a proxy to the outside.

## Who can forbid it

- Without a Pro server that lists `pro.tunnel` (public server, base server, LAN sessions) the agent does not offer tunnels and the button does not appear.
- The administrator of the Pro server: rule "No TCP tunnels" (web console, Rules; app, Administration), `Policy.no_tunnel`.
- The person at the PC: app settings → "This computer" → "Allow tunnels to the local network" (on by default), kept in `permissions.json` by the agent like the other choices.

## How it works

- **Protocol** (`proto/nuvira/v1/session.proto`). `Hello.tunnel` from the agent. Every bidirectional stream of the viewer after the control one starts with `StreamOpen` (terminal or tunnel). For a tunnel, `TunnelOpen { host, port }`; the agent answers `TunnelResult` and then the stream carries the raw bytes of the TCP connection in both directions; either side closing ends it. One stream per TCP connection, so a slow connection does not hold up the others; the server only relays the session as for everything else.
- **Agent** (`client/agent/src/tunnel.rs`). Resolves the name, keeps the local addresses, connects (10 s timeout, `TCP_NODELAY`), copies both ways.
- **Viewer** (`client/viewer/src/tunnels.rs`, `tunnels.html`, `net.rs`). A listener for each tunnel; for each connection accepted a stream (`forward`).
- **Pro server** (`nuvira-pro`, module `tunnel`). Only announces the feature when the licence includes `TUNNEL`.

## Tests

2026-11-06, Pro test server on the Ubuntu VM (licence with `TUNNEL`), agent on PC-LABORATORIO, viewer on the NUC with `--terminal --tunnel ...`:

| Tunnel | Result |
|---|---|
| 18080 → 192.168.178.1:80 (the router) | `HTTP/1.1 301 Moved Permanently`, Location https://192.168.178.1/ |
| 13389 → localhost:3389 (Remote Desktop of PC-LABORATORIO) | RDP negotiation answered: TPKT + X.224 Connection Confirm |
| 18053 → 8.8.8.8:53 | refused: "only addresses of the remote local network can be reached" |
| Rule "No TCP tunnels" on | "tunnels not available on this agent" |

Not tested yet: a full Remote Desktop session through the tunnel, the "Tunnels" window and its buttons, the app setting, throughput with large transfers, Linux and macOS as the controlled computer.
