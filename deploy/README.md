# Deploying nuvira-server

The server is published as a Docker image by GitHub Actions on every push to `main`:

```
ghcr.io/nuvira-hub/nuvira-server:latest     # also :sha-<commit> and :<version> for tags
```

Images for `linux/amd64` and `linux/arm64`, built `FROM scratch` (only the static binary).

## Container settings

| Kind | Name | Value | Notes |
|---|---|---|---|
| Port | UDP 21116 | 21116/udp | signalling (QUIC) and relay; **required** |
| Port | TCP 21116 | 21116/tcp | WebSocket fallback for networks that block UDP; status page at `http://server:21116/` (address to enter in the apps, port to forward, what to back up) |
| Volume | `/data` | e.g. `/mnt/user/appdata/nuvira-server` | `nuvira.db`, `cert.pem`/`key.pem` (server identity) |
| Variable | `NUVIRA_LOG` | `info` | `debug`, `info`, `warn` |
| Variable | `NUVIRA_WS_LISTEN` | `:21116` | `off` disables the WebSocket fallback |
| Variable | `NUVIRA_RATE_LIMITS` | unset (= `on`) | limits per address on public addresses only; `all` also on local addresses; `off` none (see [Limits per address](#limits-per-address)) |
| Variable | `NUVIRA_PUBLIC_URL` | unset | the server's address from outside, as `https://name` (the web console's address with NuviraHub Pro): the status page shows its name as the address to use from outside; Pro also uses it for the invitation links and single sign-on |

The image runs as user 65532. With a bind mount owned by another user, run the container with that user (`--user UID:GID`) or give the folder to 65532.

**Back up the whole `/data` folder.** Losing `cert.pem`/`key.pem` makes every client refuse the server (they pin its certificate at the first connection). If only one of the two files is missing, the server does not start and names it: put it back from the backup (removing both makes a new certificate, which every client then has to trust again).

This is the open server: signalling, relay and updates; each app keeps its own address book. Server address books, accounts and central rules are NuviraHub Pro modules, with their own image.

No public address has to be configured: clients reach the relay at the same address and port they use for the server.

## Limits per address

A server open to anybody protects itself per client address (an IPv4 address, or an IPv6 /64 network). The numbers are fixed:

| What | Limit | Over the limit |
|---|---|---|
| Registration of a connection | within 10 s of the connection (stream, `Register`, then the proof of the key) | closed |
| Connections open at once (registered or not) | 256 | `@server.rate_limited` at its registration |
| Registrations | 30 a minute, burst 100 | `@server.rate_limited` |
| Registrations of keys never seen (each creates a device) | 10 a minute, burst 100, per IPv6 /48 | `@server.rate_limited` |
| Connection requests | 10 a minute, burst 15 | `@server.rate_limited` |
| Presence of devices (`GetPresence`, no sign-in) | 2,048 device IDs a minute, burst 4,096, besides one request of at most 256 IDs every 2 s per connection | "too many requests" |
| WebSocket connections at once | 64 | HTTP 429 |
| Datagrams of one WebSocket connection | 10,000 a second | dropped (QUIC sends them again) |

The bursts let an office behind one address restart or install all its PCs at once (each app keeps a few connections: agent, address book, viewer during a session).

`NUVIRA_RATE_LIMITS` says to which addresses they apply:

- unset or `on` (default): public addresses only. Addresses of the server's machine and of local networks (loopback, 10/8, 172.16/12, 192.168/16, IPv6 `fc00::/7`) have no limits, so a company server on its LAN is never limited.
- `all`: local addresses too. Use it when the server sees every client with a local address: behind a userland UDP proxy (rootless Docker or Podman port forwarding, Docker's userland proxy, a UDP load balancer). Docker's default port publishing and the host network keep the real addresses: there the default is right. The start-up log says which mode is active.
- `off`: no limits at all.

WebSocket clients behind a reverse proxy on the same machine or network are counted by the address the proxy adds last to `X-Forwarded-For`.

Other protections, always active: devices **registered and never seen again** are removed from the database after 30 days (a device whose only connection lasted at most 10 minutes, with no alias, not connected, and not in an address book, history, inventory or support queue of the Pro modules); such a device coming back gets a new ID. Idle keep-alive HTTP connections are closed after 2 minutes (WebSocket tunnels are not affected). Blocks of devices, addresses and networks: `nuvira-server block ID|IP|NETWORK`, `unblock`, `blocked` (read by the running server within 30 seconds).

## docker compose

See [docker-compose.yml](docker-compose.yml).

## Behind a reverse proxy (HTTPS)

A reverse proxy in front of TCP 21116 gives the server a name with HTTPS on port 443: the status page, the WebSocket tunnel `/nuvira` for networks that block UDP (as `wss://name/nuvira`, which passes corporate proxies and firewalls), the update files and, with NuviraHub Pro, the web console (`/console/`, sign-in from the Internet only over HTTPS), the web client (`/web/`), the invitation links (`/i/`) and the REST API (`/api/v1`). Everything goes to the same port, so one rule for the whole host is enough.

**UDP 21116 does not go through the proxy** (QUIC is not HTTP): forward it from the router to the server as always. The apps keep using UDP first and the tunnel only when UDP is blocked.

Ready configurations, with the name `nuvira.example.com` to replace:

- [proxy/Caddyfile](proxy/Caddyfile): Caddy, certificate from Let's Encrypt by itself;
- [proxy/nginx.conf](proxy/nginx.conf): nginx with certbot's certificate; the WebSocket headers and the 1-hour timeouts are needed (the default 60 s cuts a quiet session);
- [proxy/traefik-dynamic.yml](proxy/traefik-dynamic.yml): Traefik v3, file provider, with your certificate resolver;
- Nginx Proxy Manager: a Proxy Host to `nuvira-server` port 21116, scheme http, **Websockets Support** on, SSL certificate from Let's Encrypt with Force SSL.

The names in the files (`nuvira-server:21116`) work with the proxy on the same Docker network as the server; with the proxy on the host use `127.0.0.1:21116`. The proxy has to send `X-Forwarded-For` (all four do): from a local address the server takes the client's address from its last entry, for the limits per address and the Pro console's allowed addresses. With NuviraHub Pro set `NUVIRA_PUBLIC_URL=https://nuvira.example.com`.

The apps use the tunnel through the proxy when told its address: unattended access `nuvira service install --server nuvira.example.com --server-ws wss://nuvira.example.com/nuvira`; for a single viewer or agent `--server-ws` on the command line. Without it the apps fall back to `ws://name:21116/nuvira`, the TCP port forwarded directly. Over `wss://` the apps accept only a certificate of a public authority (the web roots built in), as Let's Encrypt's: a self-signed certificate or a private CA is refused (`invalid peer certificate`).

Tried 2026-11-27 on the Ubuntu VM with podman, server 0.99.3 behind Caddy 2, nginx (stable) and Traefik v3, each on its own port, over HTTP and HTTPS (Caddy's internal CA, a self-signed certificate): status page 200 and WebSocket upgrade 101 through all six; a session of the 0.99.3 agent (synthetic image) and viewer, both forced onto the tunnel, through each proxy: 15 s at 30 fps, median latency 13–14 ms, no freezes, through the relay; the address of a client container reached the server through each proxy's `X-Forwarded-For`; over `wss://` with the self-signed certificate the app refused the connection, as it must. The configurations above, with the test certificates in certbot's paths: `nginx -t` and `caddy validate` pass, Traefik loads the router and answers. Nginx Proxy Manager with Let's Encrypt in front of the Pro server on the NAS (console, web client, WebSocket): docs/28-web-client.md. Not tried: these files with a real Let's Encrypt certificate (a name reachable from the Internet is needed).

## IPv6

The server and its relay work over IPv4 and IPv6 alike, and the devices use both: with a name that has both an A and an AAAA record, each device also announces itself to the relay on its second family (server 0.21 or later), so the other side learns its public addresses of both and tries them all. Over IPv6 there is no NAT, and the direct path succeeds much more often (mobile networks and CGNAT, which over IPv4 end up on the relay).

The server has to see the devices' real addresses, which a Docker bridge network does not give over IPv6 unless IPv6 is enabled in Docker: otherwise `docker-proxy` hands the packets to the container from the bridge's own address, so the relay gives the other side a useless address and the per-address limits count every IPv6 device as one. Two ways:

- **Host network** (simplest, what the public server uses): `network_mode: host` in compose and no `ports:` (on Unraid: Network Type "Host"). The server listens on port 21116 of the host, IPv4 and IPv6; open it in the host's firewall for both families.
- **IPv6 in Docker**: in `/etc/docker/daemon.json` `{"ipv6": true, "ip6tables": true}` (Docker 27 and later have `ip6tables` on by default), restart Docker, and give the container a network with IPv6 (`enable_ipv6: true` on the compose network). Docker then forwards IPv6 with the source address kept, as it does for IPv4. On Unraid: Settings → Docker → "IPv4 + IPv6" for the network type, with the Docker service stopped.

Add the AAAA record only once the server answers over IPv6 (`nc -u -6 HOST 21116` from a network with IPv6, or a device with `--server [IPv6 address]`): a device that resolves the AAAA record first may otherwise reach nothing there. Devices on an IPv4-only network keep using IPv4.

The router forwards ports only for IPv4 (NAT); over IPv6 the server's address is public, but the router's firewall must let UDP and TCP 21116 in towards it (often "Allow incoming IPv6", "Exposed host" or a "Port sharing" rule for the server's IPv6 address).

## Unraid

1. Copy [unraid/nuvira-server.xml](unraid/nuvira-server.xml) to the flash drive as `/boot/config/plugins/dockerMan/templates-user/my-nuvira-server.xml`.
2. Docker → Add Container → Template: `nuvira-server`. Check the appdata path, Apply.
3. The template adds `--user 99:100` (Extra Parameters) so the container can write to appdata, which belongs to `nobody:users`.
4. Check the log: `nuvira-server started` with the certificate fingerprint.

## TrueNAS SCALE

TrueNAS SCALE 24.10 and later run apps on Docker, so the server installs from a compose file:

1. Create a dataset for the data, e.g. `POOL/apps/nuvira-server`, and give it to the `apps` user (568): Datasets → the dataset → Permissions → Edit, User and Group `apps`, apply recursively.
2. Apps → Discover Apps → ⋮ → **Install via YAML**, name `nuvira-server`, paste [truenas/nuvira-server.yaml](truenas/nuvira-server.yaml) with your pool's name in the volume path.
3. Check the app's log: `nuvira-server started` with the certificate fingerprint. Forward UDP and TCP 21116 on the router to the NAS.

Port 21116 must be free on the NAS (another remote desktop server, RustDesk's, uses 21115–21119). For NuviraHub Pro change the image to `ghcr.io/nuvira-hub/nuvira-server-pro:latest` and add its variables. Back up the dataset (`cert.pem`/`key.pem` above all).

Tried: the image as user 568 with a folder owned by 568 (podman on the Ubuntu VM, 2026-11-27: started, certificate and database written, status page answering). Not tried on a real TrueNAS SCALE.

## Proxmox VE

The server is a container image: on Proxmox run it in a Debian LXC with Docker, or in any VM with Docker.

1. Create an unprivileged Debian 12 or 13 container with **nesting** on (Options → Features → Nesting; Docker needs it), 1 core, 512 MB of memory and 4 GB of disk are plenty; give it a fixed address.
2. In the container: `apt install docker.io docker-compose` (or Docker's own repository), then [docker-compose.yml](docker-compose.yml) with `docker compose up -d`.
3. Forward UDP and TCP 21116 on the router to the container's address.

The data stays in the Docker volume inside the container: Proxmox's backup of the container (vzdump) includes it.

Not tried on a real Proxmox VE.

## Router

Forward **UDP 21116** and **TCP 21116** from the WAN to the server. Clients use the public name or address (`--server nuvira.example.com`, or the app settings).

Devices on the same LAN as the server that use the public name need NAT reflection (hairpin) on the router, or a local DNS record that resolves the name to the server's LAN address. Direct connections between devices are not affected: they also try local addresses (see [docs/13-phase1-network.md](../docs/13-phase1-network.md)).

## Updates

The server also hands the signed app updates to its devices, from `/data/updates` over the TCP port (`http://server:21116/nuvira/update/windows-x64.json`). Devices with unattended access install a new version within 6 hours, never during a session; copies that are not installed offer it.

To publish a version: download `nuvira-update-VERSION.zip` from the [GitHub release](https://github.com/nuvira-hub/nuvira/releases/latest) and unpack it into `/data/updates` (on Unraid `/mnt/user/appdata/nuvira-server/updates`). Do not copy the zip itself there: the server only hands out the unpacked files. Its folders and `.json` files go straight into `updates`, replacing the `.json` files already there. The folder then holds one manifest and one folder per channel:

```
updates/windows-x64.json      updates/windows-x64/nuvira-VERSION.exe
updates/linux-x64.json        updates/linux-x64/nuvira-VERSION
updates/linux-x64-deb.json    updates/linux-x64-deb/nuvira-VERSION.deb
updates/linux-x64-rpm.json    updates/linux-x64-rpm/nuvira-VERSION.rpm
updates/macos.json            updates/macos/nuvira-VERSION.zip
```

`macos/nuvira-VERSION.zip` stays zipped: it is the macOS app, unpacked by the Mac when it updates. The programs of older versions can be deleted. No restart is needed: the manifest is read at every request. The files must be readable by the container's user (65532, or the one given with `--user`). Every update is signed with the NuviraHub release key and the app checks the signature before installing it, so a tampered file is refused.

With NuviraHub Pro the web console does this for you: Settings → Client updates shows the latest release, downloads it from GitHub, checks every file against the release key and publishes it in `/data/updates`; the server also checks for a new release once a day.
