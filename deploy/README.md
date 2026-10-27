# Deploying nexodesk-server

The server is published as a Docker image by GitHub Actions on every push to `main`:

```
ghcr.io/alexbn71/nexodesk-server:latest     # also :sha-<commit> and :<version> for tags
```

Images for `linux/amd64` and `linux/arm64`, built `FROM scratch` (only the static binary).

## Container settings

| Kind | Name | Value | Notes |
|---|---|---|---|
| Port | UDP 21116 | 21116/udp | signalling (QUIC) and relay; **required** |
| Port | TCP 21116 | 21116/tcp | WebSocket fallback for networks that block UDP |
| Volume | `/data` | e.g. `/mnt/user/appdata/nexodesk-server` | `nexodesk.db`, `cert.pem`/`key.pem` (server identity), `address-book.key` |
| Variable | `NEXODESK_ADMIN_USER` | `admin` | first administrator, only when no account exists |
| Variable | `NEXODESK_ADMIN_PASSWORD` | ≥ 8 characters | empty: a random password is printed once in the log |
| Variable | `NEXODESK_LOG` | `info` | `debug`, `info`, `warn` |
| Variable | `NEXODESK_WS_LISTEN` | `:21116` | `off` disables the WebSocket fallback |

The image runs as user 65532. With a bind mount owned by another user, run the container with that user (`--user UID:GID`) or give the folder to 65532.

**Back up the whole `/data` folder.** Losing `cert.pem`/`key.pem` makes every client refuse the server (they pin its certificate at the first connection); losing `address-book.key` makes the saved device passwords unreadable.

No public address has to be configured: clients reach the relay at the same address and port they use for the server.

## docker compose

See [docker-compose.yml](docker-compose.yml).

## Unraid

1. Copy [unraid/nexodesk-server.xml](unraid/nexodesk-server.xml) to the flash drive as `/boot/config/plugins/dockerMan/templates-user/my-nexodesk-server.xml`.
2. Docker → Add Container → Template: `nexodesk-server`. Set the admin password, check the appdata path, Apply.
3. The template adds `--user 99:100` (Extra Parameters) so the container can write to appdata, which belongs to `nobody:users`.
4. Check the log: `nexodesk-server started` with the certificate fingerprint.

## Router

Forward **UDP 21116** and **TCP 21116** from the WAN to the server. Clients use the public name or address (`--server nexodesk.example.com`, or the app settings).

Devices on the same LAN as the server that use the public name need NAT reflection (hairpin) on the router, or a local DNS record that resolves the name to the server's LAN address. Direct connections between devices are not affected: they also try local addresses (see [docs/13-phase1-network.md](../docs/13-phase1-network.md)).
