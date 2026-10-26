# Nexo — Funzioni innovative (non presenti in RustDesk né in Supremo)

[← Indice](README.md)

> **Rinviate a dopo la 1.0** (decisione D8): prima si costruisce la base funzionante descritta in [04-funzioni.md](04-funzioni.md).

### 5.1 Lavoro "dietro le quinte" con monitor virtuale
Il tecnico lavora su un **monitor virtuale aggiuntivo** creato al momento (IddCx / CGVirtualDisplay) mentre l'utente continua a usare il proprio schermo. Le finestre si spostano tra i due monitor. Usi:
- assistenza senza interrompere l'utente;
- PC senza monitor collegato (server, mini PC) con risoluzione scelta dal tecnico;
- **privacy mode affidabile**: si oscura lo schermo fisico e si lavora su quello virtuale.

### 5.2 Modalità semantica a banda minima
Su connessioni pessime (tethering, satellite), invece dei pixel si trasmette l'**albero di accessibilità** delle finestre (UI Automation / AX / AT-SPI): pulsanti, campi, testi. Il client ricostruisce un'interfaccia schematica cliccabile con pochi kB/s. Effetto collaterale: assistenza remota **usabile da tecnici non vedenti** con lettore di schermo.

### 5.3 Permessi granulari a tempo, decisi dall'utente
Quando il tecnico si collega, l'utente vede una richiesta del tipo:
"Mario Rossi chiede: vedere lo schermo ✔, controllare mouse ✔, file ✖, terminale ✖ — per 30 minuti".
L'utente può spuntare/negare singole capacità e il tecnico può chiederne di nuove durante la sessione. A scadenza la sessione si chiude da sola.

### 5.4 Oscuramento automatico dei dati sensibili
Il client remoto rileva campi password, finestre di home banking, notifiche private e le **sfuma prima di codificare il video** (quindi non arrivano mai al tecnico né nelle registrazioni). Regole configurabili per policy.

### 5.5 Pass di accesso temporanei
Come i pass ospiti di UHPPOTE-Desk: link o QR firmato che dà a una persona esterna (es. un fornitore) accesso a **un solo dispositivo**, con capacità limitate, finestra oraria e numero massimo di sessioni, senza creare un account.

### 5.6 Moviola: rivedere gli ultimi minuti
Il client remoto tiene in memoria un buffer circolare degli ultimi 2–5 minuti (solo se l'utente lo attiva). Il tecnico può chiedere: "fammi vedere cosa è successo prima dell'errore" — l'utente approva e il buffer viene inviato. Risolve il classico "non ricordo cosa ho cliccato".

### 5.7 Registrazioni ricercabili e report automatico
Le registrazioni vengono indicizzate (testo a schermo, finestre aperte, comandi eseguiti nel terminale, file trasferiti). Si cerca "regedit" o "stampante" e si salta al momento esatto. A fine sessione viene generato un **riepilogo automatico** (cosa è stato fatto, durata, file toccati) pronto per ticket o fattura.

### 5.8 Assistente AI durante la sessione (opzionale, disattivabile)
- Suggerisce la diagnosi leggendo i messaggi d'errore sullo schermo.
- Traduce in tempo reale la chat tra tecnico e utente di lingua diversa.
- Prepara il riepilogo di fine sessione.
- Esecuzione solo con modello scelto dall'amministratore (anche locale), mai attiva di default, con indicazione visibile all'utente remoto.

### 5.9 Cursori multipli collaborativi
Più tecnici nella stessa sessione, ciascuno con il proprio cursore colorato e nome; "passaggio del controllo" esplicito; utile per formazione e affiancamento.

### 5.10 Libreria di azioni e operazioni di massa
Script e "ricette" salvati nel server (pulizia cache stampante, reset rete, installa aggiornamento) eseguibili su un dispositivo o su un gruppo intero, con esito per dispositivo, **senza aprire il desktop**. Firma degli script e approvazione per quelli marcati sensibili.

### 5.11 Scheda di salute prima di connettersi
Nella rubrica ogni dispositivo mostra: spazio disco, ultimo riavvio, aggiornamenti in sospeso, stato antivirus, uso CPU/RAM, utente collegato. Avvisi configurabili ("disco < 10%"). Spesso il problema si capisce prima di entrare.

### 5.12 Regola dei quattro occhi
Per dispositivi critici (server, PC amministrazione) la policy richiede l'**approvazione di un secondo amministratore** prima che la sessione parta.

### 5.13 Crittografia resistente ai computer quantistici
Scambio chiavi ibrido X25519 + ML-KEM fin dal primo rilascio: le sessioni registrate oggi non sono decifrabili domani.

### 5.14 Registro di audit non alterabile
Ogni evento (connessione, permesso concesso, file trasferito, comando) è firmato e concatenato con hash; il dispositivo remoto conserva una copia locale. Una manomissione del log sul server è rilevabile.
