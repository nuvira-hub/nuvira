# NexoDesk

Sistema di controllo remoto self-hosted.

- **Server** (Go): un container Docker (rendezvous, STUN, relay, API, console di amministrazione) su una porta TCP 443 e una UDP 443.
- **Client desktop** (Rust; Tauri v2 per la gestione, finestra nativa wgpu per la sessione): Windows, Linux (X11 e Wayland), macOS — assistenza al volo e accesso non presidiato.
- **Crittografia end-to-end**: il server non vede mai schermo, input o file.

Stato: **fase 0, prototipo Windows in LAN** ([come provarlo](docs/12-fase0-prototipo.md)). Tutta la documentazione è in [docs/](docs/README.md).

## Licenza

Copyright (C) 2026 Alessandro Blason.

NexoDesk è software libero, rilasciato sotto la **[GNU Affero General Public License v3.0 o successiva](LICENSE)** (AGPL-3.0-or-later). Chi lo modifica e lo rende disponibile ad altri, anche solo via rete, deve pubblicare il sorgente della propria versione con la stessa licenza.

Come termine aggiuntivo ai sensi della sezione 7(b) della licenza, i crediti dell'autore mostrati nelle interfacce ("NexoDesk © 2026 Alessandro Blason · AGPL-3.0 · Source code") devono restare visibili e inalterati in ogni copia e versione modificata: vedi **[ADDITIONAL-TERMS.md](ADDITIONAL-TERMS.md)**.
