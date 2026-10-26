# NexoDesk — Obiettivi e vincoli

[← Indice](README.md)

1. **Un solo container** server (più eventuale DB) con **una porta TCP 443 + una porta UDP** — facile da pubblicare dietro NAT/NPM come già fatto per CasaAuto.
2. **Sicurezza end-to-end**: il server non può vedere schermo, input o file.
3. **Client unico** per Windows, Linux (X11 e Wayland), macOS, con modalità "assistenza al volo" (senza installazione) e "non presidiato" (servizio).
4. **Latenza bassa**: obiettivo < 50 ms glass-to-glass in LAN, < 120 ms su Internet con P2P.
5. **Gestione centralizzata** nel server libero: utenti, rubrica, gruppi, policy, audit.
6. Codice **scritto da zero** (RustDesk solo come riferimento di studio; vedi decisione D3).
7. **Open source AGPL-3.0-or-later** con crediti dell'autore obbligatori nelle interfacce (decisione D6).
