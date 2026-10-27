# NexoDesk — Fase 3: servizio di sistema (non presidiato)

[← Indice](README.md)

## Perché

Un agent avviato dall'utente non vede il **desktop protetto** di Windows: richieste di amministratore (UAC), schermata di blocco, Ctrl+Alt+Canc. Durante la prova del 2026-10-27 l'aggiornamento di FileZilla ha bloccato l'immagine finché qualcuno non ha risposto sul posto (vedi [08-roadmap.md](08-roadmap.md)). Il servizio risolve questo e permette l'accesso a un PC senza nessuno davanti.

## Come funziona (Windows)

- `nexodesk-service.exe` è un servizio **LocalSystem** con avvio automatico. Gira nella sessione 0, che non ha schermo.
- Ogni mezzo secondo legge la sessione della console (`WTSGetActiveConsoleSessionId`) e tiene attivo `nexodesk-agent.exe` in quella sessione:
  - l'agent parte con una copia del token SYSTEM del servizio spostata nella sessione (`SetTokenInformation(TokenSessionId)` + `CreateProcessAsUserW`, desktop `winsta0\default`);
  - a ogni cambio di sessione (disconnessione, cambio utente) l'agent viene chiuso e riavviato nella nuova;
  - se l'agent termina da solo si riavvia, con attesa crescente fino a 30 s se continua a cadere subito;
  - l'agent sta in un job object con `KILL_ON_JOB_CLOSE`: se il servizio cade, Windows chiude anche l'agent (niente copie orfane con lo stesso ID).
- **Desktop protetto**: i thread di cattura e di input seguono il desktop che riceve l'input (`OpenInputDesktop` + `SetThreadDesktop`, in `nexodesk-capture/src/desktop.rs`). Quando la duplicazione DXGI si interrompe per il cambio di desktop, viene ricreata sul nuovo. L'input passa da un thread dedicato che controlla il desktop prima di ogni evento. Solo un processo SYSTEM può aprire `Winlogon`: da utente il passaggio fallisce e la cattura riprende al ritorno sul desktop normale.
- Da SYSTEM l'input arriva anche alle finestre aperte come amministratore (niente blocco UIPI).

## File

| Percorso | Contenuto | Accesso |
|---|---|---|
| `%ProgramFiles%\NexoDesk\` | `nexodesk-service.exe`, `nexodesk-agent.exe` | scrittura solo amministratori (il servizio non deve eseguire file sostituibili da un utente) |
| `%ProgramData%\NexoDesk\service.json` | server, password permanente, trasporto | solo SYSTEM e amministratori |
| `%ProgramData%\NexoDesk\device.key` | chiave del dispositivo: l'ID resta lo stesso anche reinstallando | idem |
| `%ProgramData%\NexoDesk\online` | "ID password" scritto dall'agent appena registrato | idem |
| `%ProgramData%\NexoDesk\service.log`, `agent.log` | log (ruotati a 5 e 10 MB) | idem |

La password passa all'agent nella variabile `NEXODESK_PASSWORD`, non sulla riga di comando, e non viene scritta nei log.

## Uso

Da un prompt dei comandi da amministratore, con `nexodesk-agent.exe` nella stessa cartella:

```text
nexodesk-service install --server nexodesk.example.it [--password ...] [--server-key ...]
nexodesk-service status
nexodesk-service uninstall
```

`install` copia i programmi in Program Files, protegge la cartella dati, registra il servizio (riavvio automatico se cade), aggiunge la regola del firewall "NexoDesk agent" e stampa ID e password. Rilanciato su un servizio esistente lo aggiorna; senza `--password` mantiene quella già impostata. `uninstall` lascia programmi e dati, così reinstallando l'ID non cambia.

## Prova del 2026-10-27

Servizio installato su PC-LABORATORIO, viewer sul NUC:

- l'agent gira come `NT AUTHORITY\SYSTEM` nella sessione della console;
- registrazione sul server in 4 s dall'avvio del servizio;
- richiesta UAC aperta durante la sessione: la cattura passa al desktop `Winlogon` subito e il viewer vede la finestra "Vuoi consentire a questa app...". Chiusa la richiesta, torna su `Default` in pochi millisecondi, senza interrompere la sessione;
- latenza mediana 24 ms in LAN;
- schermata di blocco: con la sessione aperta dal NUC, LABORATORIO è stato bloccato; la schermata di blocco è comparsa nel viewer e l'utente l'ha sbloccata con il PIN da remoto. Cattura e input sono passati a `Winlogon` e tornati a `Default` dopo lo sblocco.

## Da fare

- Riavvio del PC con ripresa automatica (prima dell'accesso dell'utente).
- Ctrl+Alt+Canc da remoto (`SendSAS`, richiede il criterio "Software Secure Attention Sequence").
- App e servizio sullo stesso PC: oggi l'app avvia un suo agent con un ID diverso. L'app dovrà riconoscere il servizio e mostrarne ID e richieste di conferma (canale locale tra app e servizio).
- Conferma da parte dell'utente presente anche in modalità servizio (opzionale, per l'assistenza presidiata).
- Sessioni Desktop remoto (RDP): oggi si segue solo la console.
- Password permanente salvata come hash invece che in chiaro (va cambiato anche il controllo nell'agent).
