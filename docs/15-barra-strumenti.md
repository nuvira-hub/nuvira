# NexoDesk — Barra degli strumenti della sessione

[← Indice](README.md)

## Com'è fatta

Barra scura in alto al centro della finestra della sessione (`client/viewer/src/toolbar.rs` e `toolbar.html`):

- è una WebView (WebView2 su Windows, libreria wry, la stessa di Tauri) figlia della finestra della sessione. Funziona con entrambi i modi di disegno del viewer (wgpu e CPU) e l'HTML potrà essere riusato nel futuro client web;
- i clic arrivano al viewer come messaggi (`window.ipc.postMessage`), lo stato torna alla pagina con `nexodesk.update({...})`;
- dopo ogni clic la tastiera torna alla sessione; il mouse sopra la barra non arriva al PC remoto;
- i dati di WebView2 stanno in `%LOCALAPPDATA%\NexoDesk\WebView2` (accanto al programma, ad esempio in Program Files, spesso non si può scrivere).

## Contenuto

| Elemento | Funzione | Dove agisce |
|---|---|---|
| Pallino, nome, fps e latenza | Verde: collegamento diretto; arancione: tramite il relay del server | viewer |
| 1, 2, … | Schermo da trasmettere (solo se l'agent ne ha più di uno) | agent: `SelectDisplay` |
| Qualità | Bilanciata → Nitida → Fluida a ogni clic | agent: `SetQuality` |
| Ctrl+Alt+Canc | Come Ctrl+Alt+Fine (vedi [14-fase3-servizio.md](14-fase3-servizio.md)) | servizio |
| Occhio | Solo visualizzazione: mouse e tastiera non vengono inviati | viewer |
| Schermo intero | Entra ed esce | viewer |
| Freccia | Riduce la barra a una linguetta; un clic la riapre | viewer |
| X | Chiude la sessione | viewer |

## Messaggi di controllo nuovi (`session.proto`)

- `Displays` (agent → viewer): schermi disponibili e quello trasmesso, all'inizio e dopo ogni cambio.
- `SelectDisplay` (viewer → agent): l'agent ferma cattura e compressione, le riavvia sul nuovo schermo con un keyframe, invia `VideoFormat` e `Displays` aggiornati e aggiorna l'area dell'input. Se il nuovo schermo non si apre resta sul precedente.
- `SetQuality` (viewer → agent): cambia il tetto del bitrate adattivo e i frame al secondo, senza riavviare nulla.

| Qualità | Bitrate massimo | Frame al secondo |
|---|---|---|
| Bilanciata (predefinita) | quello dell'agent (8 Mbit/s) | quelli dell'agent (60) |
| Nitida | doppio | al più 30 |
| Fluida | metà | quelli dell'agent |

Il bitrate resta adattivo: la qualità ne sposta solo il tetto.

## Prove del 2026-10-27

- Barra, Ctrl+Alt+Canc, solo visualizzazione, schermo intero, riduci e chiudi verso PC-LABORATORIO: provati dall'utente.
- Cambio schermo e qualità: sessione dal NUC (due schermi) verso il NUC stesso in solo visualizzazione, provati dall'utente.

## Collegato

Un viewer chiuso di forza lasciava l'agent "occupato" fino al timeout di QUIC (20 s). Ora il timeout è 10 s con un pacchetto di presenza ogni 2 s (`transport/src/lib.rs`).

## Da fare

- Chat, appunti e trasferimento file (fase 2) entreranno nella barra.
- Menu a tendina (ad esempio per la qualità) richiedono di ingrandire temporaneamente la WebView: per ora la qualità cambia a ogni clic.
