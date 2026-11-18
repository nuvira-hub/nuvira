package updates

// Phase 2 (characterisation): fuzzing of the update file names.

import (
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// FuzzAllowedPath checks the path filter: a name the regexp accepts has at
// most one "/", no backslash, no leading "/", and ends as a JSON
// manifest or a nuvira-<version>[.ext] program (the regexp lets "1..0"
// through; the handler refuses ".." apart). Then the handler, on a folder
// with a secret next to it and inside it, never returns the secret.
func FuzzAllowedPath(f *testing.F) {
	for _, s := range []string{
		"windows-x64.json", "windows-x64/nuvira-0.1.0.exe", "linux-x64/nuvira-0.1.0", "macos/nuvira-0.1.0.zip",
		"../address-book.key", "windows-x64/../../address-book.key", "windows-x64%2F..%2Faddress-book.key",
		"a/nuvira-1..1", "a/nuvira-1.0.exe/x", "nuvira-1.0", "a/b/nuvira-1.0", "x.json\n", "WINDOWS.json",
		"a\\nuvira-1.0", "/etc/passwd", ".json", "-.json", "a/nuvira-1",
	} {
		f.Add(s)
	}
	dir := f.TempDir()
	data := filepath.Join(dir, "data")
	upd := filepath.Join(data, "updates")
	if err := os.MkdirAll(filepath.Join(upd, "windows-x64"), 0o755); err != nil {
		f.Fatal(err)
	}
	for name, body := range map[string]string{
		filepath.Join(data, "address-book.key"):             "SECRET-OUTSIDE",
		filepath.Join(upd, "secret.key"):                    "SECRET-INSIDE",
		filepath.Join(upd, "windows-x64", "notes.txt"):      "SECRET-NOTES",
		filepath.Join(upd, "windows-x64.json"):              "manifest",
		filepath.Join(upd, "windows-x64", "nuvira-1.0.exe"): "program",
	} {
		if err := os.WriteFile(name, []byte(body), 0o644); err != nil {
			f.Fatal(err)
		}
	}
	h := Handler(upd)
	f.Fuzz(func(t *testing.T, name string) {
		if allowed.MatchString(name) {
			if strings.Count(name, "/") > 1 || strings.HasPrefix(name, "/") || strings.ContainsAny(name, "\\\x00\n") {
				t.Fatalf("%q accepted", name)
			}
			if !strings.HasSuffix(name, ".json") && !strings.Contains(name, "/nuvira-") {
				t.Fatalf("%q accepted", name)
			}
		}
		req := &http.Request{Method: http.MethodGet, URL: &url.URL{Path: Prefix + name}, Header: http.Header{}}
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, req)
		if strings.Contains(rec.Body.String(), "SECRET") {
			t.Fatalf("%q served a secret: %d %q", name, rec.Code, rec.Body.String())
		}
		if rec.Code == http.StatusOK && (!allowed.MatchString(name) || strings.Contains(name, "..")) {
			t.Fatalf("%q served without matching", name)
		}
	})
}
