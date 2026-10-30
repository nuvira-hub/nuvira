# NuviraHub — Repository layout (monorepo)

[← Index](README.md)

```
NuviraHub/
├─ proto/nuvira/v1/      # shared Protobuf (session.proto, rendezvous.proto)
├─ server/                 # Go: nuvira-server
│  ├─ cmd/nuvira-server/ # entry point, configuration from environment variables
│  └─ internal/
│     ├─ signal/           # registration, presence, connection requests, address book API
│     ├─ relay/            # UDP relay, WebSocket fallback, network simulation for tests
│     ├─ accounts/         # accounts, address books, entries
│     ├─ store/            # devices (SQLite)
│     ├─ tlsutil/          # server certificate
│     └─ pb/               # Go code generated from proto/ (tools/gen-proto.sh)
├─ client/                 # Rust workspace
│  ├─ proto/               # protocol messages and framing (no OS dependencies)
│  ├─ transport/           # QUIC, device identity, signalling, P2P/relay, WebSocket, address book client
│  ├─ capture/             # screen capture per platform (DXGI on Windows)
│  ├─ codec/               # H.264: Media Foundation (hardware), OpenH264 (software)
│  ├─ input/               # mouse and keyboard injection per platform
│  ├─ clipboard/           # shared clipboard
│  ├─ i18n/                # interface strings in five languages
│  ├─ agent/               # controlled device: library + nuvira-agent command line
│  ├─ viewer/              # session window (wgpu or CPU) with toolbar and chat
│  ├─ app/                 # Tauri v2 app (nuvira)
│  └─ service/             # Windows system service (nuvira-service)
├─ deploy/                 # docker-compose
├─ tools/                  # LAN test script, protobuf generation
└─ docs/
```
