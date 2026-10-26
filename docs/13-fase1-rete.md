# NexoDesk — Fase 1: server e connessione via Internet

[← Indice](README.md)

Obiettivo ([08-roadmap.md](08-roadmap.md)): due dispositivi in reti diverse si collegano tramite `nexodesk-server`, in diretta (P2P) quando possibile, altrimenti attraverso il relay, con autenticazione vera al posto dell'impronta copiata a mano.

## Scelte

| # | Scelta | Motivo |
|---|---|---|
| D10 | **Crittografia end-to-end con il TLS 1.3 di QUIC tra i due dispositivi**, con certificati legati alle chiavi Ed25519 dei dispositivi (verifica reciproca), invece di un secondo handshake Noise | Un solo handshake e un solo strato di cifratura; stessa sicurezza (autenticazione reciproca, forward secrecy). Il server attesta l'associazione ID ↔ chiave pubblica |
| D11 | **Relay a livello di datagrammi UDP** (come TURN): il server inoltra i pacchetti QUIC già cifrati tra i due dispositivi | Lo stesso codice QUIC funziona diretto e via relay; il relay non può leggere nulla |
| D12 | **Una sola porta UDP sul server** per segnalazione (QUIC) e relay: i pacchetti da indirizzi registrati per il relay vengono inoltrati, gli altri vanno al QUIC del server | Una porta da aprire nel firewall/NAT (più TCP 443 per il ripiego, più avanti) |
| D13 | Password di sessione verificata dall'agent **dentro** il canale cifrato; il server non la vede mai | Passo intermedio verso il PAKE previsto in [06-sicurezza.md](06-sicurezza.md) |

## Componenti

```
            ┌──────────────────── nexodesk-server (Go) ─────────────────────┐
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

### Diretta (P2P)

Nel bind al relay ciascun lato aggiunge il proprio indirizzo locale (l'interfaccia con cui raggiunge il server, con la porta del socket della sessione). Quando entrambi sono agganciati, il relay comunica a ciascuno i candidati dell'altro:

- l'indirizzo **visto dal relay**, attraverso gli eventuali NAT (hole punching tra reti diverse);
- gli indirizzi **locali** (stessa rete dietro lo stesso router, dove l'indirizzo pubblico spesso non è raggiungibile: hairpin).

Entrambi i lati inviano pacchetti di apertura verso tutti i candidati; il viewer tenta QUIC verso ognuno in parallelo al relay e vince la prima connessione diretta. Il relay si usa solo se nessuna diretta riesce entro 1,5 s da quando il relay è pronto.

Rispetto a `rustdesk-server`, che riconosce i dispositivi dietro lo stesso router dall'IP pubblico uguale e solo allora chiede l'indirizzo locale, qui i candidati locali si provano sempre: non serve indovinare la topologia e copre anche i casi in cui l'IP pubblico visto dal server non coincide (CGNAT con più uscite, IPv4/IPv6).

Test: `NEXODESK_RELAY_FAKE_REFLEXIVE=1` sul server sostituisce l'indirizzo visto dal relay con uno irraggiungibile (203.0.113.1); la connessione deve comunque riuscire in diretta tramite l'indirizzo locale.

## Passi

| # | Passo | Stato |
|---|---|---|
| 1 | Server Go: segnalazione QUIC, registrazione con firma, SQLite, ID a 9 cifre. Agent: `--server`, stampa ID e password | ✅ |
| 2 | Richiesta di connessione, relay UDP, viewer che si collega per ID con chiavi verificate, password di sessione | ✅ |
| 3 | Test automatici via server in LAN (`NEXODESK_VIA_SERVER=1`, `NEXODESK_FORCE_RELAY=1`) | ✅ |
| 4 | Immagine Docker (`ghcr.io/alexbn71/nexodesk-server`, GitHub Actions) | ✅ immagine; pubblicazione su Internet rimandata |
| 5 | Hole punching e scelta del percorso (diretta preferita, relay come ripiego) | ✅ |
| 5b | Indirizzi locali come candidati (dispositivi dietro lo stesso router, hairpin) | ✅ |
| 6 | Ripiego su WebSocket per le reti che bloccano UDP | ✅ |

## Uso

```powershell
nexodesk-server.exe                                   # UDP 21116, dati in .\data
nexodesk-agent.exe --server 192.168.1.10              # stampa ID e password
nexodesk-viewer.exe --server 192.168.1.10 123456789 --password abcd2345
```

Al primo collegamento agent e viewer memorizzano l'impronta del certificato del server (`servers.txt` nella cartella dati) e rifiutano poi un server con un certificato diverso; `--server-key` la fissa a mano.

## Misure in LAN (2026-10-26)

NUC (agent + nexodesk-server) → PC-LABORATORIO (viewer), schermo vero con animazione, Quick Sync.

| Percorso | fps | Latenza p50 | p95 | RTT |
|---|---|---|---|---|
| Diretta | 32 | 37 ms | 46 ms | 1,3 ms |
| Relay (forzato) | 33 | 43 ms | 52 ms | 1,7 ms |

## Ripiego WebSocket (reti che bloccano UDP)

Invece di un secondo protocollo di sessione su TCP (come `hbbr` di RustDesk, che inoltra connessioni TCP/WebSocket), NexoDesk fa passare **gli stessi datagrammi QUIC dentro un WebSocket**:

- **client** (`client/transport/src/ws.rs`): un socket per quinn che invia e riceve i datagrammi attraverso il WebSocket. QUIC, crittografia end-to-end, verifica delle chiavi e relay restano identici. I tentativi di via diretta vengono scartati (su queste reti UDP non passa).
- **server** (`server/internal/relay/websocket.go`): ogni connessione WebSocket diventa un indirizzo virtuale nello stesso smistamento della porta UDP; il relay collega senza differenze un lato UDP con un lato WebSocket.
- **Scelta**: `--transport auto` (predefinito) prova UDP per 4 s e poi passa a `ws://<server>:21116/nexodesk`; `--transport udp|websocket` forza; `--server-ws wss://…` indica l'URL pubblicato dietro un reverse proxy. La sessione usa lo stesso trasporto della segnalazione.
- Server: `NEXODESK_WS_LISTEN` (predefinito `:21116` in TCP, `off` per disattivarlo), percorso `/nexodesk`.

Prove in locale (agent e viewer sullo stesso PC, 1280x720 a 30 fps):

| Agent → viewer | Primo frame | Latenza p50 | p95 |
|---|---|---|---|
| UDP → UDP | 0,8 s | 13 ms | 41 ms |
| UDP → WebSocket | 1,4 s | 14 ms | 42 ms |
| WebSocket → WebSocket | 0,6 s | 18 ms | 44 ms |
| UDP bloccato, scelta automatica | 5,5 s | 25 ms | 44 ms |

Tra i due PC (NUC → PC-LABORATORIO via nexodesk-server, Quick Sync, schermo animato):

| Viewer | Percorso | fps | Latenza p50 | p95 |
|---|---|---|---|---|
| UDP | diretto | 29 | 38 ms | 49 ms |
| WebSocket | relay via tunnel | 28 | 39 ms | 57 ms |
| Automatico con UDP bloccato | relay via tunnel | 29 | 38 ms | 50 ms |

In LAN il tunnel WebSocket costa pochissimo; via Internet il TCP sotto QUIC peggiora con le perdite (ritrasmissioni doppie), per questo resta un ripiego.

Correzioni emerse: il messaggio di chiusura di QUIC passa per una coda svuotata da un task e il viewer usciva prima che partisse (l'agent restava occupato fino al timeout); chiusura del viewer in ordine (wgpu, finestra, thread di rete e di decodifica) dopo crash sporadici in uscita. 30 sessioni consecutive alternando UDP e WebSocket senza errori.

## Rete degradata: bitrate adattivo e BBR (2026-10-26)

Simulazione sul relay (`NEXODESK_RELAY_IMPAIR`, solo test), tutto il traffico forzato attraverso il relay, schermo vero con animazione, 25 s.

| Rete simulata | Prima (bitrate fisso, Cubic) | Dopo (adattivo + BBR), hardware | Dopo, software |
|---|---|---|---|
| Libera (LAN) | 37 ms | 38 ms (p95 49) | 59 ms (p95 78) |
| 25 ms, 1% perdita, 5 Mbit/s | 2,2 s (p95 4,7 s) | **78 ms** (p95 130) | 90 ms (p95 147) |
| 40 ms, 2% perdita, 2 Mbit/s | 3,7 s (p95 9,6 s) | **100 ms** (p95 202) | 127 ms (p95 222) |

Cosa è stato fatto:
- **Feedback dal viewer** ogni 250 ms: latenza mediana e minima dei frame mostrati.
- **Bitrate adattivo** nell'agent (`client/agent/src/abr.rs`): scende del 20% se la latenza supera di 60 ms il minimo degli ultimi 10 s, risale dell'8% se resta vicina; 1,5 s di assestamento dopo ogni cambio. `--fixed-bitrate` lo disattiva.
- **Quick Sync ignora i cambi di bitrate a sessione avviata** (Media Foundation): il nuovo bitrate si applica riavviando l'encoder (~0,3 s), con isteresi (discese ≥ 15% al massimo ogni secondo, salite ≥ 25% ogni 3 s). OpenH264 cambia al volo.
- **BBR** come controllo di congestione di QUIC: con perdite casuali NewReno/Cubic usavano meno della metà della banda (`NEXODESK_CC_CUBIC=1` per confrontare).
- **Buffer di invio di QUIC limitato a 256 KB**: oltre, l'agent salta frame invece di accodarli.
- Simulatore: ritardo, jitter senza riordino, perdita, banda limitata con coda finita.

## Cosa cambia via Internet

La configurazione di OPNsense (inoltro della porta UDP del server) serve solo a rendere raggiungibile il server; il software però va ancora verificato su condizioni che la LAN non presenta:

- **NAT simmetrici e CGNAT** (reti mobili): la via diretta può fallire e si resta sul relay; da misurare quanto spesso.
- **Hairpin**: due dispositivi dietro lo stesso NAT che raggiungono il server con l'IP pubblico. Risolto con i candidati locali (sotto); verificato in LAN simulando un indirizzo pubblico irraggiungibile.
- **Reti che bloccano UDP** (aziende, alberghi): ripiego WebSocket (sotto). Per attraversare proxy e firewall aziendali va pubblicato come `wss://` sulla 443 dietro il reverse proxy.
- **Perdita di pacchetti, jitter, RTT di 30-100 ms**: affrontati con bitrate adattivo e BBR (sezione sopra), verificati in simulazione; resta la prova reale.
- **MTU** più piccole (VPN, PPPoE): QUIC lo gestisce, da verificare.

Questi punti si possono in parte simulare in LAN (perdita e ritardo artificiali) prima della prova vera.
