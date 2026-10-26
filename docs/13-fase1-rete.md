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

1. Server Go: segnalazione QUIC, registrazione con firma, SQLite, ID a 9 cifre. Agent: `--server`, stampa il proprio ID.
2. Richiesta di connessione, relay UDP, viewer che si collega per ID con chiavi verificate, password di sessione.
3. Test automatici via server (in LAN), confronto della latenza con la connessione diretta.
4. Immagine Docker e pubblicazione (NAS / server esterno).
5. Hole punching e scelta del percorso più veloce.
6. Ripiego su TCP/WebSocket per le reti che bloccano UDP.
