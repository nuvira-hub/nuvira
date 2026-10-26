# Nexo — Fase 0: prototipo Windows in LAN

[← Indice](README.md)

Obiettivo della fase ([08-roadmap.md](08-roadmap.md)): cattura, compressione con la scheda video e visualizzazione in LAN, con la latenza misurata.

## Stato

| Parte | Modulo | Stato |
|---|---|---|
| Protocollo Protobuf + framing | `client/proto` | ✅ |
| Cattura schermo DXGI Desktop Duplication | `client/capture` | ✅ compila, elenca gli schermi; da provare a schermo sbloccato |
| Sorgente sintetica per i test | `client/capture` | ✅ |
| Conversione BGRA → I420/NV12 (interi, multi-core) | `client/codec` | ✅ ~3–4 ms a 1080p |
| Encoder hardware Media Foundation (Intel Quick Sync) | `client/codec` | ✅ |
| Encoder software OpenH264 (ripiego) | `client/codec` | ✅ |
| Trasporto QUIC con impronta del certificato | `client/transport` | ✅ |
| Mouse e tastiera (SendInput, tasti W3C) | `client/input` | ✅ compila; da provare a schermo sbloccato |
| Viewer wgpu (Direct3D 12, YUV → RGB nello shader) | `client/viewer` | ✅ |
| Decodifica hardware nel viewer | — | da fare |
| Percorso senza copie: texture GPU → encoder | — | da fare |

## Come provarlo

Requisiti per compilare: Rust stable, Visual Studio Build Tools (C++). NASM è facoltativo: rende più veloce OpenH264.

```powershell
cargo build --release
```

Sul PC da controllare:

```powershell
target\release\nexo-host.exe                # schermo principale, encoder automatico
target\release\nexo-host.exe --list-displays
target\release\nexo-host.exe --display 1 --encoder software
target\release\nexo-host.exe --synthetic 1920x1080@60   # immagine generata, senza schermo
```

L'host stampa l'impronta del proprio certificato. Sul PC che controlla:

```powershell
target\release\nexo-viewer.exe 192.168.1.20 --fingerprint <impronta>
```

Il titolo della finestra mostra fps, latenza cattura→visualizzazione, RTT, banda e tempi di compressione e decompressione. La porta predefinita è **UDP 21500** (va aperta nel firewall di Windows dell'host).

> Prototipo: nessuna autenticazione oltre all'impronta, una sessione alla volta. Solo per reti fidate.

## Misure (2026-10-26)

PC di prova: Intel Core i7-10710U, Intel UHD Graphics, Windows 11. Host e viewer **sullo stesso PC** (localhost), quindi in competizione per CPU e GPU; sorgente sintetica 1920x1080 a 60 fps con una barra che si muove.

| Encoder | fps | Latenza cattura→visualizzazione | Compressione | Decompressione (OpenH264) | Banda |
|---|---|---|---|---|---|
| Hardware (Quick Sync via Media Foundation) | 20–23 | **36–54 ms** | ~20 ms (4 conversione + 7–20 encoder) | 14–25 ms | ~2,9 Mbit/s |
| Software (OpenH264) | 16–19 | 46–157 ms | 25–39 ms | 4–5 ms | ~0,2 Mbit/s |

Banco di prova degli encoder (`cargo run --release -p nexo-codec --example bench`), immagine tipo desktop 1080p:

| Encoder | ms/frame |
|---|---|
| Quick Sync, a regime | ~15 (4 conversione + 11 compressione) |
| OpenH264, modalità "camera" | 19 |
| OpenH264, modalità "contenuto schermo" | 98 (scartata) |

Osservazioni:
- Su Windows wgpu usa **Direct3D 12**: il driver Vulkan Intel di questo PC va in crash all'avvio, quindi Vulkan è escluso di default (`WGPU_BACKEND` permette comunque di forzarlo).
- Con lo schermo bloccato la cattura viene rifiutata (`E_ACCESSDENIED`): è il desktop sicuro, che si potrà catturare solo dal servizio di sistema (fase 3).
- Il codice OpenH264 è stato compilato senza NASM: con NASM ci si aspetta un miglioramento.

## Prossimi passi per arrivare a 60 fps

1. **Cattura e compressione su thread separati**, così la cattura del frame successivo avviene mentre il precedente viene compresso.
2. **Percorso senza copie sull'host**: la texture D3D11 della cattura viene convertita in NV12 dalla GPU (D3D11 Video Processor) e passata all'encoder tramite `IMFDXGIDeviceManager`. Questo elimina la conversione sulla CPU e il caricamento in memoria video.
3. **Decodifica hardware nel viewer** (Media Foundation / D3D11), con il passaggio a wgpu.
4. Misura tra **due PC diversi** in LAN e a schermo sbloccato, con la cattura reale.
