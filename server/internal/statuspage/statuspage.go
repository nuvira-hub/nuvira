// Package statuspage is the page the server shows at http://host:21116/: that it
// is running, the address to enter in the apps, the port to forward and the data
// to back up. It is what NAS dashboards (Umbrel, CasaOS, …) open with their
// "Open" button. It shows nothing about the devices or the sessions.
package statuspage

import (
	"html/template"
	"net"
	"net/http"
	"strconv"
	"strings"
)

// DefaultPort is the server's default port: the apps add it when the address
// has none.
const DefaultPort = 21116

// Pattern is the route of the page: the root only, every other path stays 404.
const Pattern = "GET /{$}"

type texts struct {
	Title, Running, Version, ConnectTitle, Install, Download, Enter, Settings, PortNote string
	OutsideTitle, Outside, BackupTitle, Backup, Console, Guide, Privacy                 string
}

// langs are the app's five languages (English first, the default).
var langs = []string{"en", "it", "fr", "de", "es"}

var all = map[string]texts{
	"en": {
		Title: "NuviraHub server", Running: "The server is running.", Version: "Version",
		ConnectTitle: "Connect your computers", Install: "Install the NuviraHub app on each computer:", Download: "download",
		Enter: "In the app, open", Settings: "Settings → Server", PortNote: "and enter this address:",
		OutsideTitle: "From outside your network",
		Outside:      "Forward port 21116 UDP from your router to this machine (and TCP too, for networks that block UDP), then enter your public address or domain name in the app.",
		BackupTitle:  "Keep a backup",
		Backup:       "Back up the server's data folder: it holds the server's identity. The apps remember it and refuse a server with a different one.",
		Console:      "Web console", Guide: "Guide",
		Privacy: "This page shows no data about your devices or sessions.",
	},
	"it": {
		Title: "Server NuviraHub", Running: "Il server è in funzione.", Version: "Versione",
		ConnectTitle: "Collega i tuoi computer", Install: "Installa l'app NuviraHub su ogni computer:", Download: "download",
		Enter: "Nell'app apri", Settings: "Impostazioni → Server", PortNote: "e inserisci questo indirizzo:",
		OutsideTitle: "Da fuori della tua rete",
		Outside:      "Inoltra la porta 21116 UDP dal router a questo computer (e anche TCP, per le reti che bloccano UDP), poi inserisci nell'app il tuo indirizzo pubblico o il tuo nome di dominio.",
		BackupTitle:  "Tieni una copia di sicurezza",
		Backup:       "Fai il backup della cartella dei dati del server: contiene l'identità del server. Le app la ricordano e rifiutano un server con un'identità diversa.",
		Console:      "Console web", Guide: "Guida",
		Privacy: "Questa pagina non mostra dati sui tuoi dispositivi né sulle sessioni.",
	},
	"fr": {
		Title: "Serveur NuviraHub", Running: "Le serveur fonctionne.", Version: "Version",
		ConnectTitle: "Connecte tes ordinateurs", Install: "Installe l'app NuviraHub sur chaque ordinateur :", Download: "téléchargement",
		Enter: "Dans l'app, ouvre", Settings: "Paramètres → Serveur", PortNote: "et saisis cette adresse :",
		OutsideTitle: "Depuis l'extérieur de ton réseau",
		Outside:      "Redirige le port 21116 UDP de ton routeur vers cette machine (et aussi TCP, pour les réseaux qui bloquent UDP), puis saisis dans l'app ton adresse publique ou ton nom de domaine.",
		BackupTitle:  "Garde une sauvegarde",
		Backup:       "Sauvegarde le dossier de données du serveur : il contient l'identité du serveur. Les apps la retiennent et refusent un serveur avec une autre identité.",
		Console:      "Console web", Guide: "Guide",
		Privacy: "Cette page ne montre aucune donnée sur tes appareils ni sur les sessions.",
	},
	"de": {
		Title: "NuviraHub-Server", Running: "Der Server läuft.", Version: "Version",
		ConnectTitle: "Verbinde deine Computer", Install: "Installiere die NuviraHub-App auf jedem Computer:", Download: "Download",
		Enter: "Öffne in der App", Settings: "Einstellungen → Server", PortNote: "und gib diese Adresse ein:",
		OutsideTitle: "Von außerhalb deines Netzes",
		Outside:      "Leite Port 21116 UDP im Router an diesen Rechner weiter (und auch TCP, für Netze, die UDP sperren), dann gib in der App deine öffentliche Adresse oder deinen Domainnamen ein.",
		BackupTitle:  "Mach eine Sicherung",
		Backup:       "Sichere den Datenordner des Servers: Er enthält die Identität des Servers. Die Apps merken sie sich und lehnen einen Server mit einer anderen ab.",
		Console:      "Webkonsole", Guide: "Anleitung",
		Privacy: "Diese Seite zeigt keine Daten über deine Geräte oder Sitzungen.",
	},
	"es": {
		Title: "Servidor NuviraHub", Running: "El servidor está en marcha.", Version: "Versión",
		ConnectTitle: "Conecta tus ordenadores", Install: "Instala la app NuviraHub en cada ordenador:", Download: "descarga",
		Enter: "En la app, abre", Settings: "Ajustes → Servidor", PortNote: "e introduce esta dirección:",
		OutsideTitle: "Desde fuera de tu red",
		Outside:      "Redirige el puerto 21116 UDP del router a esta máquina (y también TCP, para las redes que bloquean UDP), luego introduce en la app tu dirección pública o tu nombre de dominio.",
		BackupTitle:  "Guarda una copia de seguridad",
		Backup:       "Haz una copia de la carpeta de datos del servidor: contiene la identidad del servidor. Las apps la recuerdan y rechazan un servidor con otra distinta.",
		Console:      "Consola web", Guide: "Guía",
		Privacy: "Esta página no muestra datos de tus dispositivos ni de las sesiones.",
	},
}

var page = template.Must(template.New("status").Parse(`<!doctype html>
<html lang="{{.Lang}}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex"><title>{{.T.Title}}</title>
<style>
:root{color-scheme:light dark;--bg:#f6f8fa;--card:#fff;--fg:#1b2430;--muted:#5b6675;--accent:#0f7aa8;--line:#d9dee5}
@media (prefers-color-scheme:dark){:root{--bg:#0f1418;--card:#172028;--fg:#e6edf3;--muted:#9aa7b4;--accent:#4fb3de;--line:#2a3540}}
body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.55 system-ui,-apple-system,"Segoe UI",sans-serif}
main{max-width:640px;margin:0 auto;padding:32px 16px}
.card{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:20px 22px;margin-bottom:16px}
h1{font-size:1.5rem;margin:0 0 4px}h2{font-size:1.1rem;margin:0 0 8px}p{margin:0 0 8px}p:last-child{margin-bottom:0}
.ok{color:#1f9d55;font-weight:600}.muted{color:var(--muted);font-size:.9rem}a{color:var(--accent)}
code{display:inline-block;font:1.05rem ui-monospace,Consolas,monospace;background:var(--bg);border:1px solid var(--line);border-radius:6px;padding:4px 10px;user-select:all}
</style></head><body><main>
<div class="card"><h1>{{.T.Title}}</h1><p class="ok">● {{.T.Running}}</p><p class="muted">{{.T.Version}} {{.Version}}</p></div>
<div class="card"><h2>{{.T.ConnectTitle}}</h2>
<p>{{.T.Install}} <a href="https://nuvirahub.dev/{{.Lang}}/download/" rel="noopener">{{.T.Download}}</a>.</p>
<p>{{.T.Enter}} <strong>{{.T.Settings}}</strong> {{.T.PortNote}}</p><p><code>{{.Address}}</code></p></div>
<div class="card"><h2>{{.T.OutsideTitle}}</h2><p>{{.T.Outside}}</p></div>
<div class="card"><h2>{{.T.BackupTitle}}</h2><p>{{.T.Backup}}</p></div>
<p class="muted">{{if .Pro}}<a href="/console/">{{.T.Console}}</a> · {{end}}<a href="https://nuvirahub.dev/{{.Lang}}/guide/" rel="noopener">{{.T.Guide}}</a> · {{.T.Privacy}}</p>
</main></body></html>
`))

// Handler serves the page. udpPort is the server's UDP port (the one the apps
// need); pro adds the link to the web console.
func Handler(version string, udpPort int, pro bool) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		lang := Lang(r.Header.Get("Accept-Language"))
		h := w.Header()
		h.Set("Content-Type", "text/html; charset=utf-8")
		h.Set("Cache-Control", "no-store")
		h.Set("X-Content-Type-Options", "nosniff")
		h.Set("Referrer-Policy", "no-referrer")
		h.Set("X-Frame-Options", "SAMEORIGIN")
		h.Set("Content-Security-Policy", "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'; frame-ancestors 'self'")
		h.Set("Vary", "Accept-Language")
		if r.Method == http.MethodHead {
			return
		}
		t := all[lang]
		if udpPort != 0 {
			t.Outside = strings.ReplaceAll(t.Outside, strconv.Itoa(DefaultPort), strconv.Itoa(udpPort))
		}
		_ = page.Execute(w, struct {
			Lang, Version, Address string
			Pro                    bool
			T                      texts
		}{lang, version, Address(r.Host, udpPort), pro, t})
	})
}

// Address is what to enter in the app: the host the browser used, with the
// server's UDP port when it is not the default. The browser's own port is not
// used: behind a NAS dashboard's proxy it is a TCP port of the proxy, not the
// server's UDP port. IPv6 addresses keep their brackets only with a port.
func Address(hostport string, udpPort int) string {
	host, _, err := net.SplitHostPort(hostport)
	if err != nil {
		host = strings.Trim(hostport, "[]")
	}
	if host == "" {
		host = "?"
	}
	if udpPort == 0 || udpPort == DefaultPort {
		return host
	}
	return net.JoinHostPort(host, strconv.Itoa(udpPort))
}

// Lang is the first of the browser's languages that the page has, else English.
func Lang(accept string) string {
	for _, part := range strings.Split(accept, ",") {
		tag := strings.ToLower(strings.TrimSpace(strings.SplitN(part, ";", 2)[0]))
		base := strings.SplitN(tag, "-", 2)[0]
		for _, l := range langs {
			if base == l {
				return l
			}
		}
	}
	return "en"
}
