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
| Port | TCP 21116 | 21116/tcp | WebSocket fallback for networks that block UDP |
| Volume | `/data` | e.g. `/mnt/user/appdata/nuvira-server` | `nuvira.db`, `cert.pem`/`key.pem` (server identity) |
| Variable | `NUVIRA_LOG` | `info` | `debug`, `info`, `warn` |
| Variable | `NUVIRA_WS_LISTEN` | `:21116` | `off` disables the WebSocket fallback |

The image runs as user 65532. With a bind mount owned by another user, run the container with that user (`--user UID:GID`) or give the folder to 65532.

**Back up the whole `/data` folder.** Losing `cert.pem`/`key.pem` makes every client refuse the server (they pin its certificate at the first connection).

This is the open server: signalling, relay and updates; each app keeps its own address book. Server address books, accounts and central rules are NuviraHub Pro modules, with their own image.

No public address has to be configured: clients reach the relay at the same address and port they use for the server.

## docker compose

See [docker-compose.yml](docker-compose.yml).

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

## Router

Forward **UDP 21116** and **TCP 21116** from the WAN to the server. Clients use the public name or address (`--server nuvira.example.com`, or the app settings).

Devices on the same LAN as the server that use the public name need NAT reflection (hairpin) on the router, or a local DNS record that resolves the name to the server's LAN address. Direct connections between devices are not affected: they also try local addresses (see [docs/13-phase1-network.md](../docs/13-phase1-network.md)).

## Updates

The server also hands the signed app updates to its devices, from `/data/updates` over the TCP port (`http://server:21116/nuvira/update/windows-x64.json`). Devices with unattended access install a new version within 6 hours, never during a session; copies that are not installed offer it.

To publish a version: download `nuvira-update-VERSION.zip` from the [GitHub release](https://github.com/nuvira-hub/nuvira/releases/latest) and unpack it into `/data/updates` (on Unraid `/mnt/user/appdata/nuvira-server/updates`), replacing `windows-x64.json`. The folder then holds:

```
updates/windows-x64.json
updates/windows-x64/nuvira-VERSION.exe
```

The older `nuvira-*.exe` can be deleted. No restart is needed: the manifest is read at every request. The files must be readable by the container's user (65532, or the one given with `--user`). Every update is signed with the NuviraHub release key and the app checks the signature before installing it, so a tampered file is refused.
