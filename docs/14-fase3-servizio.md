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
- riavvio: LABORATORIO riavviato alle 08:11:18, servizio avviato alle 08:11:33, agent registrato alle 08:11:39 senza nessun utente entrato. Dal NUC si vede la schermata di accesso; l'utente ha inserito il PIN da remoto e la sessione è proseguita sul desktop (l'accesso non cambia la sessione della console).
- spegnimento: Windows chiude le sessioni (e l'agent, codice `0x40010004`) prima di fermare i servizi. Il servizio lo trattava come un crash e programmava un riavvio; ora lo riconosce e riparte senza avvisi solo se la sessione continua.

## Ctrl+Alt+Canc

Ctrl+Alt+Canc non si può inviare come tasti: il sistema del viewer lo intercetta sempre. Nel viewer si usa **Ctrl+Alt+Fine** (come in Desktop remoto); in futuro anche il pulsante della barra degli strumenti.

- Il viewer invia `InputEvent.secure_attention` (protocollo `session.proto`).
- Windows esegue `SendSAS` solo se lo chiama un **servizio** (l'agent, pur essendo SYSTEM, viene ignorato in silenzio) e se il criterio `SoftwareSASGeneration` (`HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System`) lo consente ai servizi. `install` aggiunge il valore 1 senza togliere quelli già presenti.
- L'agent segnala l'evento `Global\NexoDeskSAS`, creato dal servizio: con la sicurezza predefinita di un oggetto creato da SYSTEM solo SYSTEM può segnalarlo. Il servizio esegue `SendSAS(FALSE)`.
- Senza servizio (agent avviato dall'app) Ctrl+Alt+Canc non è disponibile e l'agent lo scrive nel log.

Prova del 2026-10-27: dal NUC con Ctrl+Alt+Fine compare su LABORATORIO la schermata di sicurezza in circa 240 ms; Esc riporta al desktop.

## App e servizio sullo stesso PC

Con il servizio installato l'app non avvia un proprio agent: mostra ID e password di quello del servizio e ne gestisce le richieste di conferma.

**Due password:**

| Password | Chi la vede | Conferma di chi usa il PC |
|---|---|---|
| Permanente (`install --password`, in `service.json`) | solo l'amministratore che installa | no: accesso non presidiato |
| Temporanea (generata dall'agent, "Nuova" nell'app) | chi usa il PC, nell'app | sì, entro 30 s; senza app aperta la richiesta è rifiutata subito |

La conferma arriva **dopo** il controllo della password: chi usa il PC non viene disturbato da chi sbaglia la password, e l'agent sa quale delle due è stata usata. Vale anche per l'app senza servizio (solo password temporanea, sempre con conferma). L'agent da riga di comando senza `--ipc` non chiede conferma, perché non c'è nessuno a cui chiedere.

**Canale locale** (`client/agent/src/ipc.rs`):

- l'agent del servizio (avviato con `--ipc`) crea la named pipe `\\.\pipe\nexodesk-agent-<sessione Windows>`, accessibile a SYSTEM e agli utenti interattivi (SDDL `D:P(A;;GA;;;SY)(A;;GRGW;;;IU)`); accetta solo app della propria sessione (`GetNamedPipeClientSessionId`);
- la prima istanza si crea solo se il nome è libero (`FILE_FLAG_FIRST_PIPE_INSTANCE`); poi l'agent prepara sempre l'istanza successiva prima di usare quella collegata, così il nome resta suo anche quando l'app si scollega;
- una riga JSON per messaggio: verso l'app `{"type":"info","server":...}` e `{"type":"event","event":{...}}` (gli `AgentEvent`), verso l'agent gli `AgentCommand` (`respond`, `end_session`, `new_password`);
- un'app alla volta; appena collegata riceve lo stato attuale (registrazione, password, sessione e richiesta in corso);
- l'app riconosce il servizio dal gestore servizi (consultabile anche da un utente normale) e, se il suo agent non risponde (avvio, cambio utente, aggiornamento), riprova ogni 2 s.

Prove del 2026-10-27 su PC-LABORATORIO, viewer dal NUC:

- l'app mostra ID del servizio, "Password temporanea" e la nota sull'accesso non presidiato;
- password permanente: entra senza nessuna richiesta;
- password temporanea: compare la richiesta nell'app, "Accetta" avvia la sessione;
- password temporanea con l'app chiusa: rifiutata subito, il viewer mostra "collegamento non accettato sul dispositivo";
- l'app si ricollega dopo un aggiornamento dell'agent e dopo due chiusure di fila. La prima versione non ricreava la pipe dopo la chiusura dell'app ("Accesso negato" con `FIRST_PIPE_INSTANCE`): corretto preparando l'istanza successiva in anticipo.

## Da fare

- Verificare nel canale locale che dall'altra parte ci sia davvero l'agent del servizio (processo SYSTEM in Program Files) e non un programma che ha occupato il nome della pipe prima di lui.
- Nel viewer, un messaggio "in attesa di conferma" mentre chi usa il PC decide (oggi la finestra resta in "connessione…").
- Sessioni Desktop remoto (RDP): oggi si segue solo la console.
- Password permanente salvata come hash invece che in chiaro (va cambiato anche il controllo nell'agent).
