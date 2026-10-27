# NexoDesk — Appunti, chat e file

[← Indice](README.md)

Ultima parte della fase 2 (MVP Windows).

## Appunti condivisi (testo)

Durante la sessione il testo copiato su uno dei due PC si può incollare sull'altro, in entrambi i sensi e senza comandi.

- `client/clipboard` (`nexodesk-clipboard`, libreria arboard): un thread per lato controlla gli appunti ogni 400 ms. Su Windows riconosce il cambio dal numero di sequenza (`GetClipboardSequenceNumber`) senza leggere gli appunti; altrove confronta il testo.
- Il testo nuovo viaggia nel canale di controllo (`ControlMessage.clipboard`, al massimo 1 MB) e l'altra parte lo scrive nei suoi appunti. Il testo ricevuto non viene rimandato indietro.
- Conta solo quello che si copia durante la sessione: il contenuto già presente all'avvio non viene inviato.
- L'agent li condivide solo se può controllare il PC (non con `--view-only`). Da SYSTEM, nella sessione dell'utente, usa gli stessi appunti dell'utente (appartengono alla window station `winsta0`).

Prova del 2026-10-27: NUC ↔ PC-LABORATORIO (agent del servizio), copia e incolla in entrambi i sensi, provata dall'utente.

## Chat

Chat testuale tra chi controlla e chi usa il PC.

- Protocollo: `ControlMessage.chat` nel canale di controllo, in entrambe le direzioni.
- Viewer: pulsante a fumetto nella barra; il riquadro (`chat.html`) è una seconda WebView a destra, sotto la barra. Si apre da solo quando arriva un messaggio. Mentre è aperto la tastiera serve a scrivere; un clic sull'immagine la riporta alla sessione.
- Al PC controllato la chat è nell'app NexoDesk, sotto "Sessione attiva": un messaggio in arrivo porta l'app in primo piano con il cursore nel campo di risposta. Funziona con l'agent dell'app e con quello del servizio (eventi `chat` e comando `chat` del canale locale, vedi [14-fase3-servizio.md](14-fase3-servizio.md)).
- La chat resta visibile nell'app anche a sessione chiusa, fino alla sessione successiva. Non viene salvata.
- L'agent da riga di comando, senza app, scrive i messaggi ricevuti nel log.

Prova del 2026-10-27: messaggio dal NUC arrivato nell'app di PC-LABORATORIO (servizio), risposta da lì arrivata nel riquadro del viewer.

## Trasferimento file

Da fare.
