# Nexo — Protocollo di connessione (bozza)

[← Indice](README.md)

Bozza dei flussi principali. I nomi dei messaggi sono indicativi: la definizione vera sarà in `proto/`.

## 1. Identità

- **Dispositivo**: coppia di chiavi Ed25519 generata al primo avvio. `device_id` = numero di 9 cifre assegnato dal server, legato alla chiave pubblica.
- **Utente tecnico**: account sul server (password + TOTP/passkey). Le sessioni partono sempre da un dispositivo, l'account serve per rubrica, policy e audit.
- **Server**: chiave Ed25519 propria; firma le associazioni `device_id ↔ chiave pubblica` (certificato del dispositivo).

## 2. Registrazione del dispositivo

```
Client                                   Server
  │── Hello{pubkey, os, versione} ─────────►│
  │◄──────── Challenge{nonce} ───────────────│
  │── Proof{firma(nonce)} ─────────────────►│
  │◄──── Registered{device_id, cert} ────────│   cert = firma server su (device_id, pubkey)
```

Il dispositivo mantiene poi una connessione di presenza (QUIC o WebSocket su 443) con keepalive ogni 25 s, usata anche per ricevere le richieste di connessione.

## 3. Avvio di una sessione

```
Tecnico (A)              Server                      Remoto (B)
  │── ConnectRequest{B, capacità richieste} ──►│
  │                          │── Incoming{A, cert A, candidati A} ──►│
  │                          │◄── Accept{candidati B} / Reject ──────│   (conferma utente o policy)
  │◄── Candidates{B, cert B, token relay} ─────│
  │                                                                  │
  │═════════ tentativi in parallelo: LAN → P2P UDP → relay ═════════│
  │═════════ handshake Noise IK + ML-KEM (chiavi dei cert) ═════════│
  │═════════ PAKE se serve password non presidiata ═════════════════│
  │═════════ SessionStart{capacità concesse, scadenza} ═════════════│
```

- **Candidati**: indirizzi LAN, indirizzo riflesso (STUN), eventuale mappatura UPnP/NAT-PMP.
- Il primo percorso che completa l'handshake vince; gli altri vengono chiusi. Il relay viene aperto subito in parallelo per non perdere tempo se il P2P fallisce.
- Il server vede solo che A e B hanno avviato una sessione, quando e per quanto: niente contenuti.

## 4. Canali dentro la sessione (stream QUIC)

| Canale | Tipo | Contenuto |
|---|---|---|
| `control` | stream affidabile | capacità, richieste permessi, cambio monitor/qualità, statistiche, chiusura |
| `input` | stream affidabile, alta priorità | mouse, tastiera, rotella, touch |
| `video` | datagrammi | frame codificati frammentati, richieste di keyframe |
| `audio` | datagrammi | Opus |
| `clipboard` | stream affidabile | testo, immagini, elenchi file |
| `file` | uno stream per trasferimento | blocchi con hash, ripresa da offset |
| `tunnel` | uno stream per connessione | port forwarding TCP |
| `terminal` | stream affidabile | PTY |

Con il ripiego su TCP/WebSocket gli stessi canali sono multiplexati su un'unica connessione con priorità (input prima di tutto).

## 5. Capacità e permessi

Capacità richiedibili: `view`, `control`, `clipboard`, `file`, `audio`, `terminal`, `tunnel`, `system` (riavvio, servizi), `record`.

- Le capacità effettive sono l'intersezione di: richiesta del tecnico ∩ policy del server ∩ consenso dell'utente remoto.
- Il dispositivo remoto applica la decisione finale: anche un server compromesso non può concedere più di quanto l'utente/la policy locale consenta.
- Ogni cambio di capacità genera un evento di audit firmato da entrambi i lati.

## 6. Versioning

- `Hello` e `SessionStart` portano la versione del protocollo; si usa la più alta comune.
- Campi Protobuf solo aggiunti, mai riutilizzati; messaggi sconosciuti ignorati.
