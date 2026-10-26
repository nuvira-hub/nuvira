# NexoDesk — Fase 0: prototipo Windows in LAN

[← Indice](README.md)

Obiettivo della fase ([08-roadmap.md](08-roadmap.md)): cattura, compressione con la scheda video e visualizzazione in LAN, con la latenza misurata.

## Stato

| Parte | Modulo | Stato |
|---|---|---|
| Protocollo Protobuf + framing | `client/proto` | ✅ |
| Cattura schermo DXGI Desktop Duplication | `client/capture` | ✅ provata sullo schermo reale |
| Sorgente sintetica per i test | `client/capture` | ✅ |
| Conversione BGRA → I420/NV12 (interi, multi-core) | `client/codec` | ✅ ~3–4 ms a 1080p |
| Encoder hardware Media Foundation (Intel Quick Sync) | `client/codec` | ✅ |
| Encoder software OpenH264 (ripiego) | `client/codec` | ✅ |
| Trasporto QUIC con impronta del certificato | `client/transport` | ✅ |
| Mouse e tastiera (SendInput, tasti W3C) | `client/input` | ✅ compila; da provare a schermo sbloccato |
| Viewer wgpu (Direct3D 12, YUV → RGB nello shader) | `client/viewer` | ✅ |
| Viewer con disegno su CPU per schede solo OpenGL | `client/viewer` | ✅ 6 ms a 1080p su HD 2500 |
| Cattura e compressione su due thread | `client/agent` | ✅ |
| Immagine sempre sulla GPU: cattura → Video Processor (NV12) → Quick Sync | `client/capture`, `client/codec` | ✅ conversione ~1 ms |
| Scelta automatica dell'encoder (prova all'avvio) | `client/codec` | ✅ |
| Test automatici tra due PC | `tools/lan-test.sh` | ✅ |
| Decodifica hardware nel viewer | — | da fare |

## Come provarlo

Requisiti per compilare: Rust stable, Visual Studio Build Tools (C++) e, consigliato, **NASM** nel `PATH` durante la compilazione (OpenH264 passa da 19 a 9 ms per frame). L'installer di NASM lo mette in `%LOCALAPPDATA%\bin\NASM` senza aggiungerlo al `PATH`.

```powershell
cargo build --release
```

Sul PC da controllare:

```powershell
target\release\nexodesk-agent.exe                # schermo principale, encoder automatico
target\release\nexodesk-agent.exe --list-displays
target\release\nexodesk-agent.exe --display 1 --encoder software
target\release\nexodesk-agent.exe --synthetic 1920x1080@60   # immagine generata, senza schermo
```

L'agent stampa l'impronta del proprio certificato. Sul PC che controlla:

```powershell
target\release\nexodesk-viewer.exe 192.168.1.20 --fingerprint <impronta>
```

Il titolo della finestra mostra fps, latenza cattura→visualizzazione, RTT, banda e tempi di compressione e decompressione. La porta predefinita è **UDP 21500** (va aperta nel firewall di Windows dell'agent).

> Prototipo: nessuna autenticazione oltre all'impronta, una sessione alla volta. Solo per reti fidate.

## Test automatico tra due PC

`tools/lan-test.sh [secondi] [opzioni dell'agent]` avvia l'agent su questo PC e il viewer su un altro PC via SSH (nella sessione Windows dell'utente, tramite un'attività pianificata), poi raccoglie log e risultati JSON in `target/lan-test/`. Esempio:

```bash
tools/lan-test.sh 15 --synthetic 1920x1080@60 --encoder software
NEXODESK_VIEWER_LOG="info,nexodesk_viewer::render=trace" tools/lan-test.sh 10
```

Opzioni usate dallo script: viewer `--duration`, `--stats-file`; agent `--once`, `--fingerprint-file`. Prerequisiti: server OpenSSH sul PC remoto con la chiave `~/.ssh/nexo_test`, regola del firewall per `dist/nexodesk-agent.exe` (UDP 21500, solo LAN).

## Misure in LAN con lo schermo vero (2026-10-26, sera)

Agent su PC-STUDIO-NUC, viewer su PC-LABORATORIO (disegno su CPU), schermo del NUC 1920x1080 con un'animazione a schermo intero (`NEXODESK_ANIMATE=1`), Supremo chiuso.

| Encoder | fps | Latenza p50 | p95 | Compressione | Banda |
|---|---|---|---|---|---|
| Hardware, immagine sempre sulla GPU | 32 | **38 ms** | 46 ms | 16 ms | 3,3 Mbit/s |
| Software (OpenH264 + NASM) | 40 | 55 ms | 70 ms | 25 ms | 1,2 Mbit/s |

Problemi trovati e corretti in questa serie:
- **Lock del device condiviso**: `AcquireNextFrame` teneva il lock del device D3D11 per tutta l'attesa (fino a 100 ms), bloccando conversione e compressione (fino a 360 ms per frame). In modalità GPU la cattura interroga senza attendere, ogni millisecondo.
- **Limitatore di fps**: la pausa di 16,7 ms partiva dalla fine della compressione invece che dall'inizio.
- **Immagine invecchiata in attesa dell'encoder**: ora si attende che Quick Sync sia pronto e solo dopo si prende l'immagine più recente (latenza da 53 a 38 ms).

Limite attuale con l'hardware: Quick Sync comprime un frame in ~13 ms ma ne chiede uno nuovo solo ~30 ms dopo il precedente, quindi ~32 fps. Con Chrome aperto sul NUC (30% del motore 3D) i tempi peggiorano.

## Misure in LAN, sorgente sintetica (2026-10-26)

Agent su PC-STUDIO-NUC (i7-10710U, Intel UHD), viewer su PC-LABORATORIO (Intel HD Graphics 2500, OpenGL perché senza Direct3D 12), rete cablata, sorgente sintetica 1920x1080@60, 15 s.

| Encoder | fps | Latenza p50 | p95 | Compressione | Decompressione | Disegno | Banda |
|---|---|---|---|---|---|---|---|
| Hardware (Quick Sync) | 21 | 51 ms | 62 ms | 18 ms | 13 ms | 24 ms | 2,4 Mbit/s |
| Software (OpenH264 + NASM) | 27 | **39 ms** | 51 ms | 11 ms | 5 ms | 24 ms | 0,3 Mbit/s |

Problemi emersi solo tra due PC e corretti:
- **Latenza in crescita** (fino a 2,5 s): il viewer mostrava tutti i frame in coda anche quando era più lento dell'agent. Ora mostra sempre l'ultimo frame decompresso e scarta quelli superati.
- **Disegno lento su OpenGL** (47 ms per frame): con `Fifo` si aspettava il vsync prima e dopo ogni frame. Senza `Mailbox` si usa `Immediate` (24 ms, qualche strappo possibile).
- Schede senza compute shader: il viewer chiede solo i limiti minimi (WebGL2).

## Misure in locale (2026-10-26)

PC di prova: Intel Core i7-10710U, Intel UHD Graphics, Windows 11. Agent e viewer **sullo stesso PC** (localhost), quindi in competizione per CPU e GPU.

### Schermo reale

Schermo principale 1920x1080 con la finestra del viewer visibile (quindi sempre in cambiamento), OpenH264 compilato con NASM.

| Encoder | fps | Latenza cattura→visualizzazione | Compressione | Decompressione |
|---|---|---|---|---|
| Hardware (Quick Sync) | 18–22 | **32–40 ms** | 18–22 ms | 7–10 ms |
| Software (OpenH264 + NASM) | 20–27 | **22–33 ms** | 11–19 ms | 4–5 ms |

### Sorgente sintetica

Immagine 1920x1080 a 60 fps con una barra che si muove, OpenH264 senza NASM.

| Encoder | fps | Latenza cattura→visualizzazione | Compressione | Decompressione (OpenH264) | Banda |
|---|---|---|---|---|---|
| Hardware (Quick Sync via Media Foundation) | 20–23 | **36–54 ms** | ~20 ms (4 conversione + 7–20 encoder) | 14–25 ms | ~2,9 Mbit/s |
| Software (OpenH264) | 16–19 | 46–157 ms | 25–39 ms | 4–5 ms | ~0,2 Mbit/s |

Banco di prova degli encoder (`cargo run --release -p nexodesk-codec --example bench`), immagine tipo desktop 1080p:

| Encoder | ms/frame |
|---|---|
| Quick Sync, a regime | ~15 (4 conversione + 11 compressione) |
| OpenH264, modalità "camera" | 19 |
| OpenH264, modalità "contenuto schermo" | 98 (scartata) |

Osservazioni:
- Su Windows wgpu usa **Direct3D 12**: il driver Vulkan Intel di questo PC va in crash all'avvio, quindi Vulkan è escluso di default (`WGPU_BACKEND` permette comunque di forzarlo).
- Con lo schermo bloccato la cattura viene rifiutata (`E_ACCESSDENIED`): è il desktop sicuro, che si potrà catturare solo dal servizio di sistema (fase 3).
- OpenH264 senza NASM: 19 ms per frame nel banco di prova; con NASM: 9 ms.
- **QUIC e GSO su Windows**: inviando più datagrammi con una sola chiamata (segmentation offload) i pacchetti in loopback andavano persi e la connessione smetteva di ricevere (circa 4 sessioni su 10 senza video). GSO è disattivato in `client/transport`; da riprovare tra due PC in LAN.

## Prossimi passi

Fatti: thread separati, percorso senza copie, misure tra due PC con lo schermo vero.

1. **Ritmo di Quick Sync** (~30 ms tra un frame e l'altro): provare più frame in volo nell'MFT, oppure l'API Intel diretta (oneVPL) o un encoder D3D12 Video.
2. **Decodifica hardware nel viewer** (Media Foundation / D3D11), utile sui PC con Direct3D 12.
3. Codifica solo delle zone cambiate e qualità adattiva alla banda (fase 1, insieme al trasporto via Internet).
