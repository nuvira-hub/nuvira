# Nexo — Modello di sicurezza (sintesi)

[← Indice](README.md)

- Ogni dispositivo genera la propria coppia di chiavi al primo avvio; la privata non lascia mai il dispositivo (Windows DPAPI/TPM, Keychain macOS, Secret Service/TPM su Linux).
- Il server conosce solo: ID, chiave pubblica, presenza, metadati di sessione (chi/quando/quanto). **Mai** contenuti.
- Password non presidiata verificata con **PAKE** (OPAQUE/SPAKE2): la password non transita, nemmeno cifrata.
- Limiti di tentativi e blocco progressivo per ID (contro brute force), avviso all'utente remoto.
- Indicatore sempre visibile di sessione attiva sul PC remoto, non disattivabile in modalità presidiata.
- Aggiornamenti automatici firmati (Ed25519) distribuiti dal proprio server.
