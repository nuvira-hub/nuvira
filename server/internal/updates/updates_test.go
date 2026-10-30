package updates

import (
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
)

func TestHandler(t *testing.T) {
	dir := t.TempDir()
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	must(os.MkdirAll(filepath.Join(dir, "windows-x64"), 0o755))
	must(os.WriteFile(filepath.Join(dir, "windows-x64.json"), []byte(`{"version":"0.1.0"}`), 0o644))
	must(os.WriteFile(filepath.Join(dir, "windows-x64", "nuvira-0.1.0.exe"), []byte("program"), 0o644))
	// Next to the updates folder, as in the server's data.
	must(os.WriteFile(filepath.Join(dir, "address-book.key"), []byte("secret"), 0o600))

	h := Handler(dir)
	get := func(method, path string) *httptest.ResponseRecorder {
		rec := httptest.NewRecorder()
		h.ServeHTTP(rec, httptest.NewRequest(method, path, nil))
		return rec
	}
	for path, want := range map[string]int{
		Prefix + "windows-x64.json":                         http.StatusOK,
		Prefix + "windows-x64/nuvira-0.1.0.exe":             http.StatusOK,
		Prefix + "windows-x64/nuvira-0.2.0.exe":             http.StatusNotFound,
		Prefix + "macos.json":                               http.StatusNotFound,
		Prefix + "address-book.key":                         http.StatusNotFound,
		Prefix + "../address-book.key":                      http.StatusNotFound,
		Prefix + "windows-x64/../../address-book.key":       http.StatusNotFound,
		Prefix + "windows-x64%2F..%2F..%2Faddress-book.key": http.StatusNotFound,
		Prefix + "WINDOWS-X64.json":                         http.StatusNotFound,
	} {
		if rec := get(http.MethodGet, path); rec.Code != want {
			t.Errorf("GET %s: %d, want %d", path, rec.Code, want)
		}
	}
	rec := get(http.MethodGet, Prefix+"windows-x64/nuvira-0.1.0.exe")
	if rec.Body.String() != "program" || rec.Header().Get("Content-Length") != "7" {
		t.Errorf("program: %q, Content-Length %q", rec.Body.String(), rec.Header().Get("Content-Length"))
	}
	if get(http.MethodGet, Prefix+"windows-x64.json").Header().Get("Cache-Control") != "no-cache" {
		t.Error("the manifest must not be cached")
	}
	if code := get(http.MethodPost, Prefix+"windows-x64.json").Code; code != http.StatusMethodNotAllowed {
		t.Errorf("POST: %d", code)
	}
}
