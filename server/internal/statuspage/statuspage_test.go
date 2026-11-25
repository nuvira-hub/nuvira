package statuspage

import (
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
)

func TestAddress(t *testing.T) {
	for _, c := range []struct {
		host string
		port int
		want string
	}{
		{"nas.local:21116", 21116, "nas.local"},
		{"nas.local", 21116, "nas.local"},
		{"umbrel.local:21180", 21116, "umbrel.local"}, // the proxy's TCP port is not the UDP one
		{"192.168.1.5:8080", 21200, "192.168.1.5:21200"},
		{"[fd00::5]:21116", 21116, "fd00::5"},
		{"[fd00::5]:80", 21200, "[fd00::5]:21200"},
		{"", 21116, "?"},
	} {
		if got := Address(c.host, c.port); got != c.want {
			t.Errorf("Address(%q, %d) = %q, want %q", c.host, c.port, got, c.want)
		}
	}
}

func TestLang(t *testing.T) {
	for accept, want := range map[string]string{
		"":                        "en",
		"it-IT,it;q=0.9,en;q=0.8": "it",
		"pt-BR,de;q=0.7":          "de",
		"FR":                      "fr",
		"ja,zh;q=0.5":             "en",
		"es-419, en":              "es",
		"en-GB,en;q=0.9":          "en",
	} {
		if got := Lang(accept); got != want {
			t.Errorf("Lang(%q) = %q, want %q", accept, got, want)
		}
	}
}

// Every language has every text.
func TestTexts(t *testing.T) {
	if len(all) != len(langs) {
		t.Fatalf("%d languages, want %d", len(all), len(langs))
	}
	for _, l := range langs {
		v := reflect.ValueOf(all[l])
		for i := range v.NumField() {
			if v.Field(i).String() == "" {
				t.Errorf("%s: %s is empty", l, v.Type().Field(i).Name)
			}
		}
	}
}

func serve(t *testing.T, mux *http.ServeMux, method, path, host, accept string) (*http.Response, string) {
	t.Helper()
	req := httptest.NewRequest(method, path, nil)
	req.Host = host
	if accept != "" {
		req.Header.Set("Accept-Language", accept)
	}
	rec := httptest.NewRecorder()
	mux.ServeHTTP(rec, req)
	body, _ := io.ReadAll(rec.Result().Body)
	return rec.Result(), string(body)
}

func TestHandler(t *testing.T) {
	mux := http.NewServeMux()
	mux.Handle("/nuvira", http.NotFoundHandler()) // the WebSocket route, as in the server
	mux.Handle("/console/", http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) { io.WriteString(w, "console") }))
	mux.Handle(Pattern, Handler("1.2.3", 21116, false, ""))

	res, body := serve(t, mux, "GET", "/", "nas.local:21116", "it-IT,it")
	if res.StatusCode != 200 || !strings.Contains(res.Header.Get("Content-Type"), "text/html") {
		t.Fatalf("status %d, type %q", res.StatusCode, res.Header.Get("Content-Type"))
	}
	for _, want := range []string{`<html lang="it">`, "Il server è in funzione.", "Versione 1.2.3", "<code>nas.local</code>",
		"Impostazioni → Server", "https://nuvirahub.dev/it/download/"} {
		if !strings.Contains(body, want) {
			t.Errorf("page without %q", want)
		}
	}
	if strings.Contains(body, "/console/") {
		t.Error("open edition links the console")
	}
	if csp := res.Header.Get("Content-Security-Policy"); !strings.Contains(csp, "default-src 'none'") {
		t.Errorf("CSP %q", csp)
	}
	// The favicon is inline (the page serves no other file), so the CSP allows data: images only.
	if !strings.Contains(body, `<link rel="icon" type="image/svg+xml" href="data:image/svg+xml,`) {
		t.Error("no inline favicon")
	}
	if csp := res.Header.Get("Content-Security-Policy"); !strings.Contains(csp, "img-src data:;") {
		t.Errorf("CSP %q does not allow the inline favicon", csp)
	}

	// The Host header is the browser's, so it can be anything: it is escaped.
	_, body = serve(t, mux, "GET", "/", `<script>alert(1)</script>`, "")
	if strings.Contains(body, "<script>") {
		t.Error("host not escaped")
	}

	// Only the root: the rest is 404 (or another route), HEAD has no body.
	for _, p := range []string{"/x", "/favicon.ico", "/index.html"} {
		if res, _ := serve(t, mux, "GET", p, "nas", ""); res.StatusCode != 404 {
			t.Errorf("%s: status %d, want 404", p, res.StatusCode)
		}
	}
	if _, body := serve(t, mux, "GET", "/console/", "nas", ""); body != "console" {
		t.Errorf("/console/ answered %q", body)
	}
	if res, body := serve(t, mux, "HEAD", "/", "nas", ""); res.StatusCode != 200 || body != "" {
		t.Errorf("HEAD: status %d, body %q", res.StatusCode, body)
	}
	if res, _ := serve(t, mux, "POST", "/", "nas", ""); res.StatusCode != 405 {
		t.Errorf("POST: status %d, want 405", res.StatusCode)
	}

	pro := http.NewServeMux()
	pro.Handle(Pattern, Handler("1.2.3", 21200, true, ""))
	_, body = serve(t, pro, "GET", "/", "192.168.1.5:21116", "de")
	for _, want := range []string{`href="/console/"`, "Webkonsole", "<code>192.168.1.5:21200</code>", "Port 21200 UDP"} {
		if !strings.Contains(body, want) {
			t.Errorf("pro page without %q", want)
		}
	}
}

// With NUVIRA_PUBLIC_URL the page also shows the address from outside, once.
func TestHandlerPublic(t *testing.T) {
	mux := http.NewServeMux()
	mux.Handle(Pattern, Handler("1.2.3", 21116, true, PublicHost("https://Nuvira.Example.net/")))
	_, body := serve(t, mux, "GET", "/", "192.168.178.12:21116", "it")
	for _, want := range []string{"<code>192.168.178.12</code>", "Inoltra la porta 21116 UDP", "Da fuori della tua rete, inserisci nell&#39;app questo indirizzo:", "<code>nuvira.example.net</code>"} {
		if !strings.Contains(body, want) {
			t.Errorf("LAN page without %q", want)
		}
	}
	if strings.Contains(body, "il tuo indirizzo pubblico o il tuo nome di dominio") {
		t.Error("generic text shown with the public address known")
	}
	// Opened from outside: the same address, shown once.
	_, body = serve(t, mux, "GET", "/", "NUVIRA.example.net", "en")
	if strings.Count(body, "<code>") != 1 || !strings.Contains(body, "The address above works from outside your network too.") {
		t.Errorf("public page: %d addresses", strings.Count(body, "<code>"))
	}
	// Another UDP port: in both addresses and in the port to forward.
	other := http.NewServeMux()
	other.Handle(Pattern, Handler("1.2.3", 21216, false, "nuvira.example.net"))
	_, body = serve(t, other, "GET", "/", "nas.local", "en")
	for _, want := range []string{"<code>nas.local:21216</code>", "<code>nuvira.example.net:21216</code>", "Forward port 21216 UDP"} {
		if !strings.Contains(body, want) {
			t.Errorf("port 21216 page without %q", want)
		}
	}
}

func TestPublicHost(t *testing.T) {
	for raw, want := range map[string]string{
		"https://nuvira.example.net":          "nuvira.example.net",
		" https://nuvira.example.net:8443/x ": "nuvira.example.net",
		"http://[2001:db8::1]:8080":           "2001:db8::1",
		"":                                    "",
		"nuvira.example.net":                  "",
		"ftp://nuvira.example.net":            "",
	} {
		if got := PublicHost(raw); got != want {
			t.Errorf("PublicHost(%q) = %q, want %q", raw, got, want)
		}
	}
}
