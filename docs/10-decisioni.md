# Nexo — Registro delle decisioni

[← Indice](README.md)

Ogni decisione presa resta qui con data e motivazione. Quelle aperte sono in fondo.

## Decise

| # | Data | Decisione | Motivazione |
|---|---|---|---|
| D1 | 2026-10-26 | Nome del prodotto: **Nexo** | Breve, neutro, da "nesso/connessione"; adatto anche a un uso commerciale |
| D2 | 2026-10-26 | Fuori ambito per ora: client web e client mobile | Concentrarsi su server Docker + client desktop Windows/Linux/macOS. Il client web resta previsto in futuro (vedi D9) |
| D3 | 2026-10-26 | Codice scritto da zero, RustDesk solo come riferimento di studio | Codice pulito e coerente. Con la licenza AGPL (D6) riusare singole parti di RustDesk è legalmente compatibile, ma va deciso caso per caso e citato nei crediti |
| D4 | 2026-10-26 | Server in **Go** | Esperienza già fatta con RustDex, binario statico, immagine Docker piccola |
| D5 | 2026-10-26 | Interfaccia **ibrida**: Tauri v2 per le finestre di gestione, finestra nativa wgpu (+ egui per la barra strumenti) per la sessione remota. Cattura/codec/input nel core Rust con API native | La webview non è adatta al video in tempo reale (soprattutto WebKitGTK su Linux); vedi [03-architettura.md](03-architettura.md#32-client-desktop) |
| D6 | 2026-10-26 | Licenza **AGPL-3.0-or-later** + termine aggiuntivo sezione 7(b): crediti "Nexo © 2026 Alessandro Blason · AGPL-3.0 · Source code" obbligatori nelle interfacce | Stesso schema di UHPPOTE-Desk; vedi [ADDITIONAL-TERMS.md](../ADDITIONAL-TERMS.md) |
| D7 | 2026-10-26 | Prima piattaforma: **Windows** | Base di utenti più ampia; Linux e macOS nella fase 4 |
| D8 | 2026-10-26 | Nessuna funzione innovativa nella prima versione | Prima una base funzionante e stabile; le innovazioni di [05-innovazioni.md](05-innovazioni.md) si aggiungono dopo la 1.0 |
| D9 | 2026-10-26 | Architettura predisposta per un futuro client web (solo controllo): crate condivisi compilabili in WebAssembly, trasporto astratto, relay con WebTransport/WebSocket, H.264 sempre disponibile | Il client web potrà riusare interfaccia di gestione e finestra di sessione (wgpu/egui in WASM); vedi [03-architettura.md](03-architettura.md#35-predisposizione-per-un-futuro-client-web) |

## Aperte

| # | Domanda | Opzioni | Proposta |
|---|---|---|---|
| A6 | Database di default | SQLite · PostgreSQL | SQLite, PostgreSQL opzionale |
| A7 | Ordine delle funzioni innovative dopo la 1.0 | vedi [05-innovazioni.md](05-innovazioni.md) | da decidere a 1.0 rilasciata |
