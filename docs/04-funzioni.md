# NexoDesk — Funzioni — elenco completo

[← Indice](README.md)

Legenda priorità: **M** = MVP, **1** = versione 1.0, **2** = versioni successive.

### 4.1 Connessione e accesso
| Funzione | Priorità |
|---|---|
| ID numerico + password monouso generata a ogni avvio | M |
| Conferma dell'utente remoto con indicazione di chi si collega | M |
| Accesso non presidiato con password permanente / chiave del dispositivo | M |
| Connessione P2P con hole punching + relay di ripiego | M |
| Connessione diretta in LAN senza server (discovery mDNS) | 1 |
| Account tecnico con 2FA (TOTP), passkey/FIDO2 | 1 |
| SSO OIDC (Google, Microsoft Entra, Authentik, Keycloak) | 2 |
| Più tecnici sulla stessa sessione | 1 |
| Wake-on-LAN tramite un altro dispositivo della stessa LAN | 1 |
| Riconnessione automatica dopo riavvio del PC remoto | M |

### 4.2 Controllo remoto
| Funzione | Priorità |
|---|---|
| Visualizzazione e controllo mouse/tastiera | M |
| Multi-monitor (singolo, tutti, schede separate) | M |
| Mappatura tastiera e combinazioni speciali (Ctrl+Alt+Canc, Win, Cmd) | M |
| Desktop sicuro / UAC / schermata di login (Windows), login macOS | 1 |
| Scala, adattamento finestra, schermo intero, risoluzione remota | M |
| Qualità/codec/fps regolabili, statistiche in tempo reale | M |
| Solo visualizzazione | M |
| Privacy mode (schermo remoto oscurato, input locale bloccato) | 1 |
| Riavvio remoto con riconnessione, riavvio in modalità provvisoria | 1 |
| Audio remoto | 1 |
| Microfono del tecnico verso il PC remoto (chiamata vocale) | 2 |

### 4.3 Dati
| Funzione | Priorità |
|---|---|
| Appunti sincronizzati (testo, immagini) | M |
| Copia/incolla di file tramite appunti | 1 |
| Gestore file a doppio pannello, trascina e rilascia, ripresa trasferimenti | M |
| Stampa remota (documento del PC remoto sulla stampante del tecnico) | 2 |
| Port forwarding / tunnel TCP (es. RDP, SSH, interfaccia web di un router) | 1 |

### 4.4 Strumenti senza video
| Funzione | Priorità |
|---|---|
| Terminale remoto (PowerShell/cmd, bash/zsh) | 1 |
| Gestione processi e servizi | 2 |
| Informazioni di sistema e inventario hardware/software | 1 |
| Visualizzatore registro eventi / journal | 2 |

### 4.5 Gestione (console del server)
| Funzione | Priorità |
|---|---|
| Rubrica personale e condivisa, gruppi, tag, ricerca | M |
| Utenti, ruoli, permessi per gruppo di dispositivi | 1 |
| Policy centralizzate (es. "conferma sempre richiesta", "niente trasferimento file") | 1 |
| Report sessioni (chi, cosa, quando, durata) esportabili CSV per fatturazione | 1 |
| Registrazione sessioni (lato client o forzata da policy) | 1 |
| Generatore di installer personalizzati (server, chiave, logo, gruppo predefinito) | 1 |
| Installazione di massa (MSI/GPO, pkg, deb/rpm) | 1 |
| Stato online/offline e ultimo accesso dei dispositivi | M |
| Webhook e API per ticketing/automazioni (Node-RED, Home Assistant) | 2 |

### 4.6 Comunicazione
| Funzione | Priorità |
|---|---|
| Chat durante la sessione | M |
| Crediti "NexoDesk © 2026 Alessandro Blason · AGPL-3.0 · Source code" nel client e nella console (obbligo di licenza) | M |
| Lavagna/annotazioni sullo schermo remoto | 2 |
