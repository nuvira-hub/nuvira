# NexoDesk — Analisi dei prodotti di riferimento

[← Indice](README.md)

### 1.1 RustDesk

**Architettura**
- Server open source in due processi:
  - `hbbs` (rendezvous / ID server): registrazione ID, scambio indirizzi per NAT traversal, chiave pubblica del server. Porte 21115 TCP, 21116 TCP+UDP, 21118 WebSocket.
  - `hbbr` (relay): inoltra il traffico quando la connessione diretta fallisce. Porte 21117 TCP, 21119 WebSocket.
- Client in Rust (core) + Flutter (UI). Protocollo su Protobuf.
- Connessione: tentativo P2P con UDP/TCP hole punching → fallback su relay. Crittografia end-to-end (NaCl, chiavi Ed25519/Curve25519).
- Codec: VP8, VP9, AV1 software; H.264/H.265 con encoder hardware (NVENC, QSV, AMF, VideoToolbox, VAAPI).

**Funzioni principali**
- ID + password (monouso o permanente), accesso non presidiato tramite servizio.
- Multi-monitor, scelta qualità/codec, adattamento banda.
- Trasferimento file, appunti (testo, immagini, file su Windows), audio remoto.
- Chat, registrazione sessione lato client, port forwarding / tunnel TCP, tunnel RDP.
- Privacy mode (schermo remoto oscurato, solo Windows, con limiti).
- Connessioni in LAN senza server (discovery), Wake-on-LAN tramite peer.
- Client personalizzabile (server e chiave incorporati nel nome file o in config).

**Limiti rilevanti**
- Server OSS minimale: **niente console web, rubrica, utenti, gruppi, policy, log centralizzati** — sono nella versione Pro a pagamento (per questo esiste il nostro RustDex).
- Licenza **AGPL-3.0**: si può studiare, ma copiare codice obbliga a rilasciare il derivato in AGPL.
- Wayland: supporto parziale (richiede conferma del portale a ogni sessione, niente non presidiato pulito).
- macOS: gestione permessi (Registrazione schermo, Accessibilità) fragile per l'utente finale.
- Audit e controllo granulare dei permessi limitati; nessuna gestione centralizzata delle policy nella versione libera.

### 1.2 Supremo (NanoSystems)

**Architettura**
- Prodotto commerciale chiuso, server cloud del produttore (nessun self-hosting reale).
- Eseguibile leggero, avviabile senza installazione; installabile come servizio per l'accesso non presidiato.
- Crittografia AES-256, conferma dell'utente, 2FA sull'account.

**Funzioni principali**
- ID + password, zero configurazione di rete.
- Accesso non presidiato, gestione UAC.
- Rubrica con gruppi sincronizzata (Supremo Console), installazioni di massa.
- Trasferimento file con gestore a doppio pannello, chat, più connessioni contemporanee.
- Report delle sessioni (chi, quando, quanto) utili per fatturazione e assistenza.
- Personalizzazione con logo/testi del rivenditore.
- Licenze per sessioni concorrenti.

**Limiti rilevanti**
- Niente self-hosting: i metadati di sessione passano dai server del produttore.
- Costo a licenza, funzioni chiuse, nessuna API aperta paragonabile a un sistema self-hosted.
- Supporto Linux/macOS meno curato rispetto a Windows.

### 1.3 Cosa prendiamo da ciascuno

| Da RustDesk | Da Supremo |
|---|---|
| Self-hosting, P2P + relay, E2E | Semplicità d'uso per l'utente finale (un clic, nessuna domanda) |
| Codec hardware, prestazioni | Console centrale con rubrica, gruppi, report |
| Port forwarding, LAN discovery | Report sessioni per fatturazione |
| Client multipiattaforma | Personalizzazione/branding del client |

**Posizionamento di NexoDesk**: la potenza e il self-hosting di RustDesk, la semplicità e la gestione centralizzata di Supremo, **già incluse nel server libero**, più funzioni che nessuno dei due offre ([05-innovazioni.md](05-innovazioni.md)).
