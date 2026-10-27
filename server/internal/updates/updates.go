// Package updates serves the program versions the devices update to, from the
// `updates` folder of the server's data, as written by `nexodesk-release sign`:
//
//	updates/windows-x64.json                 signed manifest
//	updates/windows-x64/nexodesk-0.1.0.exe   the program
//
// The server only hands the files out: the devices check the manifest signature
// (NexoDesk release key, built into the program) and the program's size and SHA-256.
// Whoever runs the server decides when its devices update by copying the files.
package updates

import (
	"net/http"
	"path/filepath"
	"regexp"
	"strings"
)

// Prefix of the update URLs, next to the WebSocket fallback (/nexodesk).
const Prefix = "/nexodesk/update/"

// Only these names are served: nothing else in the data folder can be reached.
var allowed = regexp.MustCompile(`^(?:[a-z0-9-]+\.json|[a-z0-9-]+/nexodesk-[0-9][0-9.]*\.exe)$`)

// Handler serves the files of dir.
func Handler(dir string) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet && r.Method != http.MethodHead {
			w.Header().Set("Allow", "GET, HEAD")
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
			return
		}
		name := strings.TrimPrefix(r.URL.Path, Prefix)
		if !allowed.MatchString(name) || strings.Contains(name, "..") {
			http.NotFound(w, r)
			return
		}
		if strings.HasSuffix(name, ".json") {
			// The manifest changes in place when a version is published.
			w.Header().Set("Cache-Control", "no-cache")
		}
		// ServeFile answers 404 for a missing file and sets Content-Length (the
		// devices' client does not accept chunked responses).
		http.ServeFile(w, r, filepath.Join(dir, filepath.FromSlash(name)))
	})
}
