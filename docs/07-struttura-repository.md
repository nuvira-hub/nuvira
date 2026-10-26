# NexoDesk — Struttura del repository (monorepo)

[← Indice](README.md)

```
NexoDesk/
├─ proto/                  # Protobuf condivisi (versionati)
├─ server/                 # Go: signal, stun, relay, api, audit
│  └─ console/             # SPA amministrazione (servita dal server)
├─ client/
│  ├─ core/                # Rust: sessione, crittografia, trasporto QUIC
│  ├─ capture/             # Rust: cattura schermo per piattaforma
│  ├─ codec/               # Rust: encoder/decoder HW e SW
│  ├─ input/               # Rust: iniezione input per piattaforma
│  ├─ service/             # Rust: servizio di sistema
│  ├─ drivers/             # IddCx (Windows) per monitor virtuale
│  ├─ viewer/              # Rust: finestra sessione nativa (wgpu + egui)
│  └─ app/                 # Tauri v2: finestre di gestione
├─ deploy/                 # Dockerfile, docker-compose, esempi NPM
└─ docs/
```
