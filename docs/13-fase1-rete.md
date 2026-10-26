# Nexo — Fase 1: server e connessione via Internet

[← Indice](README.md)

Obiettivo ([08-roadmap.md](08-roadmap.md)): due dispositivi in reti diverse si collegano tramite `nexo-server`, in diretta (P2P) quando possibile, altrimenti attraverso il relay, con autenticazione vera al posto dell'impronta copiata a mano.

## Scelte

| # | Scelta | Motivo |
|---|---|---|
| D10 | **Crittografia end-to-end con il TLS 1.3 di QUIC tra i due dispositivi**, con certificati legati alle chiavi Ed25519 dei dispositivi (verifica reciproca), invece di un secondo handshake Noise | Un solo handshake e un solo strato di cifratura; stessa sicurezza (autenticazione reciproca, forward secrecy). Il server attesta l'associazione ID ↔ chiave pubblica |
| D11 | **Relay a livello di datagrammi UDP** (come TURN): il server inoltra i pacchetti QUIC già cifrati tra i due dispositivi | Lo stesso codice QUIC funziona diretto e via relay; il relay non può leggere nulla |
| D12 | **Una sola porta UDP sul server** per segnalazione (QUIC) e relay: i pacchetti da indirizzi registrati per il relay vengono inoltrati, gli altri vanno al QUIC del server | Una porta da aprire nel firewall/NAT (più TCP 443 per il ripiego, più avanti) |
| D13 | Password di sessione verificata dall'agent **dentro** il canale cifrato; il server non la vede mai | Passo intermedio verso il PAKE previsto in [06-sicurezza.md](06-sicurezza.md) |

## Componenti

```
            ┌──────────────────── nexo-server (Go) ─────────────────────┐
            │  UDP :21116                                                 │
            │   ├─ demux: indirizzo legato a una sessione relay? ─► relay  │
            │   └─ altrimenti ─► QUIC (quic-go): segnalazione             │
            │  SQLite: dispositivi (ID, chiave pubblica, ultimo accesso)  │
            └─────────────────────────────────────────────────────────────┘
               ▲ presenza + richieste            ▲ datagrammi relay
   agent ──────┘                                  └────── viewer
     │◄═══════════ QUIC end-to-end (TLS con chiavi dei dispositivi) ══════►│
```

- **Identità del dispositivo**: coppia di chiavi Ed25519 creata al primo avvio e salvata nella cartella dati dell'utente. Il certificato TLS del dispositivo è autofirmato con questa chiave.
- **ID**: 9 cifre assegnate dal server alla prima registrazione, legate alla chiave pubblica.

## Flussi

### Registrazione e presenza (agent)

```
agent                                server
  │── Register{pubkey, nome, os} ──────►│
  │◄────────────── Challenge{nonce} ────│
  │── Proof{firma Ed25519(nonce)} ─────►│  verifica firma; ID esistente o nuovo
  │◄──── Registered{device_id} ─────────│
  │      (connessione aperta: keepalive QUIC, richieste in arrivo)
```

### Connessione (viewer → agent)

```
viewer                         server                          agent
  │── Connect{ID, pubkey viewer} ─►│                               │
  │                                │── Incoming{sessione, pubkey ──►│
  │                                │   viewer, nome viewer}         │
  │                                │◄── Accept{sessione} ───────────│
  │◄── Route{sessione, relay,      │                               │
  │    pubkey agent}               │                               │
  │  (ciascuno apre un socket UDP e manda RelayBind{sessione, ruolo} al relay)
  │════ QUIC viewer→relay→agent, certificati verificati sulle pubkey ═══│
  │── Auth{password} (nel canale cifrato) ──────────────────────────►  │
```

Il relay accoppia i due indirizzi che hanno inviato `RelayBind` per la stessa sessione e da quel momento inoltra ogni datagramma dall'uno all'altro.

### Diretta (P2P), passo successivo

Il server comunica a entrambi l'indirizzo pubblico dell'altro (visto dal relay); i due lati provano la connessione diretta in parallelo al relay e si tiene la prima che riesce.

## Passi

| # | Passo | Stato |
|---|---|---|
| 1 | Server Go: segnalazione QUIC, registrazione con firma, SQLite, ID a 9 cifre. Agent: `--server`, stampa ID e password | ✅ |
| 2 | Richiesta di connessione, relay UDP, viewer che si collega per ID con chiavi verificate, password di sessione | ✅ |
| 3 | Test automatici via server in LAN (`NEXO_VIA_SERVER=1`, `NEXO_FORCE_RELAY=1`) | ✅ |
| 4 | Immagine Docker (`ghcr.io/alexbn71/nexo-server`, GitHub Actions) | ✅ immagine; pubblicazione su Internet rimandata |
| 5 | Hole punching e scelta del percorso (diretta preferita, relay come ripiego) | ✅ |
| 6 | Ripiego su TCP/WebSocket per le reti che bloccano UDP | da fare |

## Uso

```powershell
nexo-server.exe                                   # UDP 21116, dati in .\data
nexo-agent.exe --server 192.168.1.10              # stampa ID e password
nexo-viewer.exe --server 192.168.1.10 123456789 --password abcd2345
```

Al primo collegamento agent e viewer memorizzano l'impronta del certificato del server (`servers.txt` nella cartella dati) e rifiutano poi un server con un certificato diverso; `--server-key` la fissa a mano.

## Misure in LAN (2026-10-26)

NUC (agent + nexo-server) → PC-LABORATORIO (viewer), schermo vero con animazione, Quick Sync.

| Percorso | fps | Latenza p50 | p95 | RTT |
|---|---|---|---|---|
| Diretta | 32 | 37 ms | 46 ms | 1,3 ms |
| Relay (forzato) | 33 | 43 ms | 52 ms | 1,7 ms |

## Cosa cambia via Internet

La configurazione di OPNsense (inoltro della porta UDP del server) serve solo a rendere raggiungibile il server; il software però va ancora verificato su condizioni che la LAN non presenta:

- **NAT simmetrici e CGNAT** (reti mobili): la via diretta può fallire e si resta sul relay; da misurare quanto spesso.
- **Hairpin**: due dispositivi dietro lo stesso NAT che raggiungono il server con l'IP pubblico; il relay li vede con l'indirizzo pubblico e la via diretta deve usare quello locale (servono anche i candidati LAN).
- **Reti che bloccano UDP** (aziende, alberghi): serve il passo 6.
- **Perdita di pacchetti, jitter, RTT di 30-100 ms**: il video oggi viaggia su uno stream affidabile, quindi una perdita blocca i frame successivi; servono datagrammi o scarto dei frame vecchi e bitrate adattivo.
- **MTU** più piccole (VPN, PPPoE): QUIC lo gestisce, da verificare.

Questi punti si possono in parte simulare in LAN (perdita e ritardo artificiali) prima della prova vera.
