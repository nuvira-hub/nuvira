# Nexo — Rischi e costi da prevedere

[← Indice](README.md)

- **Firma del codice**: Windows (certificato di firma, meglio EV, altrimenti SmartScreen avvisa); macOS (Apple Developer 99 $/anno + notarizzazione obbligatoria). Il driver IddCx richiede firma Microsoft (attestation signing tramite Partner Center).
- **Wayland**: il non presidiato su GNOME/KDE richiede il portale RemoteDesktop con token di ripristino; comportamento diverso tra compositor.
- **macOS**: i permessi Registrazione schermo e Accessibilità vanno concessi a mano dall'utente; serve una procedura guidata chiara.
- **Brevetti codec**: H.264/H.265 usati solo tramite encoder di sistema/hardware; AV1 e VP9 liberi.
- **Licenza**: scelta AGPL-3.0-or-later + crediti obbligatori (D6). Il termine 7(b) obbliga a mantenere i crediti, ma non impedisce a terzi di offrire Nexo come servizio: possono farlo pubblicando il sorgente delle loro modifiche. Se un giorno servisse una versione commerciale chiusa, è possibile solo finché tutto il codice è dell'autore (attenzione a contributi esterni: valutare un accordo di contribuzione, CLA).
- **Abuso**: gli strumenti di controllo remoto sono usati nelle truffe ("finto tecnico"). Contromisure: avviso chiaro all'utente in modalità presidiata, nessuna modalità nascosta, segnalazione ID abusivi.
