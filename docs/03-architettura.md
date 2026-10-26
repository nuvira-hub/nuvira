# Nexo — Architettura

[← Indice](README.md)

```
                    ┌──────────────────── Container "nexo-server" ────────────────────┐
                    │                                                                    │
 Client tecnico ───►│  Gateway TLS :443 (HTTP/2, WebSocket, QUIC su UDP :443/udp)        │
                    │   ├─ Rendezvous / Signaling   (presenza, scambio candidati NAT)    │
                    │   ├─ STUN integrato           (scoperta indirizzo pubblico)        │
                    │   ├─ Relay                    (QUIC/TCP/WSS, cieco: solo cifrato)  │
                    │   ├─ API REST + gRPC          (dispositivi, rubrica, policy)       │
                    │   └─ Console web di amministrazione (non è un client remoto)       │
                    │                                                                    │
                    │  Storage: SQLite (default) o PostgreSQL · Registrazioni: volume/S3 │
 Client assistito ─►│                                                                    │
                    └────────────────────────────────────────────────────────────────────┘
           ▲                                                                   
           └──────────── P2P diretto (QUIC con hole punching) quando possibile ───────────┘
```

### 3.1 Server (Docker)

- **Linguaggio**: Go (decisione D4; stessa esperienza di RustDex, binario statico, immagine `distroless` < 30 MB). Il codice del protocollo viene generato da `proto/` sia per Go sia per Rust.
- **Moduli**, tutti nello stesso binario, attivabili/disattivabili:
  - `signal` — presenza dispositivi (connessione persistente WebSocket/QUIC), scambio candidati ICE-like, notifiche push al dispositivo da raggiungere.
  - `stun` — risposta indirizzo riflesso sulla porta UDP.
  - `relay` — accoppia due flussi cifrati per token di sessione; limiti di banda per tenant/utente; più istanze relay possibili in regioni diverse.
  - `api` — REST/JSON + OpenAPI; autenticazione utenti (password + TOTP + passkey/FIDO2 + OIDC per SSO).
  - `console` — SPA di amministrazione servita dal binario (rubrica, dispositivi, gruppi, policy, report, registrazioni, pacchetti di installazione personalizzati).
  - `audit` — log a catena di hash (non alterabile senza che si veda).
- **Configurazione**: variabili d'ambiente + file YAML; generazione automatica della chiave del server al primo avvio.
- **TLS**: Let's Encrypt integrato oppure dietro reverse proxy (NPM) con QUIC passato direttamente.
- **Multi-tenant** opzionale (per un tecnico che gestisce più aziende clienti).
- **Migrazione** da RustDex: importazione della rubrica esistente.

`docker-compose.yml` di riferimento:

```yaml
services:
  nexo:
    image: nexo/server:latest
    ports:
      - "443:443/tcp"
      - "443:443/udp"
    volumes:
      - ./data:/data
    environment:
      NEXO_PUBLIC_HOST: remote.example.it
      NEXO_DB: sqlite:///data/nexo.db
    restart: unless-stopped
```

### 3.2 Client desktop

- **Core in Rust** (prestazioni, sicurezza della memoria, ottime librerie per QUIC, codec, crittografia, cross-compilazione).
- **Cattura, codifica, decodifica e input sono tutti nel core Rust**, con le API native di ogni sistema (tabella più sotto). Il framework dell'interfaccia non partecipa: la qualità della cattura non dipende da Tauri.
- **Interfaccia ibrida** (decisione D5):
  - **Finestre di gestione con Tauri v2**: rubrica, impostazioni, ID/password, conferma connessione, chat, trasferimento file, informazioni. Un solo codice UI per i tre sistemi, binario piccolo.
  - **Finestra della sessione remota nativa, senza webview**: finestra `tao`/`winit` + rendering **wgpu** (Direct3D 12 / Metal / Vulkan) che mostra i frame appena decodificati in hardware, senza copie in memoria e senza passare dalla webview. Barra degli strumenti della sessione disegnata nella stessa finestra (egui sopra wgpu).
  - Motivo: la webview è il punto debole per il video in tempo reale. WebView2 e Safari reggerebbero (WebCodecs), ma su Linux WebKitGTK non ha decodifica hardware affidabile; ogni frame passato dal core alla webview aggiunge copie e latenza. Con la finestra nativa la latenza è la stessa su tutti e tre i sistemi.
  - Alternative valutate: tutto in Flutter (scelta di RustDesk, rendering video tramite texture nativa, ma un secondo linguaggio, Dart); tutto in Slint/egui (nessuna webview, interfaccia di gestione più faticosa da costruire e meno curata).
  - Tauri v2 supporta Windows, macOS e Linux con lo stesso codice; usa la webview di sistema, da cui derivano questi requisiti minimi:

    | Sistema | Webview usata da Tauri | Versione minima di Nexo | Limite dato da |
    |---|---|---|---|
    | Windows | WebView2 (Edge, già presente su Windows 10/11; installabile dall'installer) | Windows 10 1903 | Windows.Graphics.Capture |
    | macOS | WKWebView | macOS 12.3 | ScreenCaptureKit (Tauri da solo arriverebbe a 10.15) |
    | Linux | WebKitGTK 4.1 | Ubuntu 22.04, Debian 12, Fedora 36 o equivalenti | WebKitGTK 4.1 richiesto da Tauri v2 |

  - Le differenze tra webview (Edge/Safari/WebKitGTK) riguardano solo l'interfaccia: va provata sui tre sistemi a ogni rilascio.
  - **Crediti** (licenza, [ADDITIONAL-TERMS.md](../ADDITIONAL-TERMS.md)): "Nexo © 2026 Alessandro Blason · AGPL-3.0 · Source code" visibili nella finestra principale e nella finestra "Informazioni".
- **Processi**:
  - `nexo` — applicazione utente (UI, sessioni in uscita, modalità "assistenza al volo").
  - `nexo-service` — servizio di sistema per il non presidiato:
    - Windows: servizio LocalSystem che lancia l'agente nella sessione attiva e sul **desktop sicuro** (UAC, schermata di accesso, Ctrl+Alt+Canc).
    - macOS: LaunchDaemon + LaunchAgent (schermata di login inclusa).
    - Linux: unità systemd; sessione grafica via portale o display manager.

| Funzione | Windows | macOS | Linux |
|---|---|---|---|
| Cattura schermo | Windows.Graphics.Capture / DXGI Desktop Duplication | ScreenCaptureKit | PipeWire (portale ScreenCast) su Wayland, XShm/XDamage su X11 |
| Input | SendInput (+ servizio per desktop sicuro) | CGEvent (permesso Accessibilità) | portale RemoteDesktop / libei su Wayland, XTest su X11, uinput come ripiego |
| Encoder HW | NVENC, Intel QSV, AMD AMF, Media Foundation | VideoToolbox | VAAPI, NVENC |
| Audio | WASAPI loopback | ScreenCaptureKit audio | PipeWire |
| Monitor virtuale | driver IddCx | CGVirtualDisplay | output virtuale del compositor / xrandr |

### 3.3 Trasporto e protocollo

- **QUIC** (libreria `quinn` nel client, `quic-go` nel server) come trasporto unico:
  - stream separati per controllo, input, appunti, file → un file grande non blocca il mouse;
  - datagrammi non affidabili per il video → niente ritardi da ritrasmissione;
  - migrazione di connessione (Wi-Fi → 4G senza cadere).
- **Ordine di tentativo**: LAN diretta → P2P con hole punching UDP → relay QUIC su UDP 443 → relay TCP/WebSocket su 443 (reti aziendali che bloccano UDP).
- **Messaggi**: Protobuf, con versione di protocollo negoziata.
- **Crittografia end-to-end**: handshake **Noise** (IK/XX) tra i due dispositivi, chiavi di identità Ed25519, **scambio ibrido X25519 + ML-KEM** (resistente a computer quantistici). Il server firma solo l'associazione ID ↔ chiave pubblica; i client verificano l'impronta (TOFU + avviso se cambia).

### 3.4 Video

- Codec negoziato: **H.264 / H.265 / AV1 hardware** quando disponibile, AV1 o VP9 software come ripiego.
- Codifica solo delle aree cambiate, bitrate e frame rate adattivi in base a RTT e perdita.
- **Modalità testo nitido**: per uffici/terminali, chroma 4:4:4 o codifica lossless dei riquadri statici.
- Decodifica hardware lato client, rendering wgpu, cursore disegnato localmente (latenza percepita zero).

### 3.5 Predisposizione per un futuro client web

Il client web è fuori ambito per ora (D2), ma l'architettura è pensata per aggiungerlo senza riscritture (D9). Il client web potrà solo **controllare** altri dispositivi: un browser non può essere controllato da remoto né fare da agente non presidiato.

**Cosa si riusa**

| Parte | Desktop | Browser | Riuso |
|---|---|---|---|
| Finestre di gestione | Tauri v2 (HTML/TS) | stessa interfaccia servita come pagina web | quasi totale: cambiano solo le chiamate al core (comandi Tauri → modulo WASM) |
| Finestra della sessione | wgpu + egui nativi | wgpu + egui compilati in WebAssembly, disegno su canvas WebGPU (WebGL2 come ripiego) | stesso codice Rust |
| Protocollo, crittografia, stato sessione | crate Rust | stessi crate compilati in WebAssembly | stesso codice Rust |
| Decodifica video | hardware nativo | **WebCodecs** (decodifica hardware del browser) | cambia solo il modulo di decodifica |
| Trasporto | QUIC (P2P o relay) | **WebTransport** (HTTP/3) o WebSocket verso il relay | cambia solo il modulo di trasporto |

**Regole da rispettare fin dal primo codice**

1. I crate `proto`, `crypto` e `session` non dipendono dal sistema operativo né da thread/socket diretti: devono compilare anche per `wasm32-unknown-unknown`. Le parti specifiche (cattura, codec, input, rete) stanno dietro interfacce (trait) con un'implementazione per piattaforma.
2. Il trasporto è un'astrazione unica (stream affidabili + datagrammi) con tre implementazioni: QUIC, WebTransport, WebSocket. Il protocollo della sessione non sa quale sta usando.
3. Il relay del server accetta fin da subito anche **WebTransport e WebSocket** sulla porta 443 (in Go: `quic-go` + `webtransport-go`). Costo minimo oggi, nessuna modifica al server domani.
4. Il dispositivo controllato offre sempre almeno un codec decodificabile da tutti i browser: **H.264** (oltre a H.265/AV1/VP9 quando entrambi i lati li supportano).
5. Crittografia end-to-end identica: Noise + ML-KEM in WebAssembly. Il server resta cieco anche per il client web. La chiave privata del browser va in IndexedDB non estraibile (WebCrypto) o in una passkey.

**Limiti noti del browser** (accettati, da documentare all'utente)

- Niente connessione diretta P2P: il browser passa sempre dal relay (salvo aggiungere in futuro WebRTC data channel, che consente l'hole punching).
- Combinazioni di tasti catturate dal browser (Ctrl+W, Ctrl+T, tasto Windows): gestibili solo a schermo intero con la Keyboard Lock API (Chrome/Edge); su Firefox e Safari servono pulsanti nella barra strumenti.
- Appunti: solo testo e immagini, con permesso del browser; niente copia/incolla di file (si usa il trasferimento file).
- Requisiti: browser recente con WebCodecs e WebTransport (Chrome/Edge; Firefox e Safari hanno WebCodecs, WebTransport va verificato al momento dello sviluppo; altrimenti si ripiega su WebSocket).
