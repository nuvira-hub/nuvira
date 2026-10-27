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

Da fare.

## Trasferimento file

Da fare.
