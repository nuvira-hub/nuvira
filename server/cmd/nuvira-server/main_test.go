package main

// Phase 2 (characterisation): the maintenance subcommands and the healthcheck
// of the open server, called in-process (os.Stdout / os.Stderr swapped).

import (
	"bufio"
	"context"
	"crypto/tls"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/quic-go/quic-go"

	"github.com/nuvira-hub/nuvira/server/internal/limits"
	"github.com/nuvira-hub/nuvira/server/internal/relay"
	"github.com/nuvira-hub/nuvira/server/internal/store"
	"github.com/nuvira-hub/nuvira/server/internal/tlsutil"
)

// tempDir is t.TempDir without failing on a removal the antivirus delays
// (Windows: it keeps scanning the fresh database for a moment).
func tempDir(t *testing.T) string {
	t.Helper()
	dir, err := os.MkdirTemp("", "nuvira-p2-main-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		for range 20 {
			if os.RemoveAll(dir) == nil {
				return
			}
			time.Sleep(100 * time.Millisecond)
		}
	})
	return dir
}

// capture runs f with os.Stdout and os.Stderr sent to files and returns them.
func capture(t *testing.T, f func() int) (code int, stdout, stderr string) {
	t.Helper()
	dir := tempDir(t)
	out, _ := os.Create(filepath.Join(dir, "out"))
	errf, _ := os.Create(filepath.Join(dir, "err"))
	oldOut, oldErr := os.Stdout, os.Stderr
	os.Stdout, os.Stderr = out, errf
	code = f()
	os.Stdout, os.Stderr = oldOut, oldErr
	_ = out.Close()
	_ = errf.Close()
	o, _ := os.ReadFile(out.Name())
	e, _ := os.ReadFile(errf.Name())
	return code, string(o), string(e)
}

func TestBlockCommands(t *testing.T) {
	dir := tempDir(t)
	t.Setenv("NUVIRA_DATA_DIR", dir)
	run := func(name string, args ...string) (int, string, string) {
		return capture(t, func() int { return command(name, args) })
	}
	if code, out, _ := run("blocked"); code != 0 || strings.TrimSpace(out) != "no blocks" {
		t.Fatalf("empty list: %d %q", code, out)
	}
	for _, c := range []struct{ in, want string }{
		{"123 456 789", "123456789"},
		{"203.0.113.7", "203.0.113.7/32"},
		{"::ffff:198.51.100.2", "198.51.100.2/32"},
		{"2001:db8::77/48", "2001:db8::/48"},
	} {
		code, out, errs := run("block", c.in, "a", "note")
		if code != 0 || strings.TrimSpace(out) != "blocked: "+c.want {
			t.Fatalf("block %q: %d %q %q", c.in, code, out, errs)
		}
	}
	// Same value again: still one row (upsert), note replaced.
	if code, _, _ := run("block", "203.0.113.7", "second"); code != 0 {
		t.Fatal("block again")
	}
	code, out, _ := run("blocked")
	lines := strings.Split(strings.TrimSpace(out), "\n")
	if code != 0 || len(lines) != 4 || !strings.Contains(out, "second") {
		t.Fatalf("list: %d %q", code, out)
	}
	for _, bad := range [][]string{{"block"}, {"block", "12345"}, {"block", "example.com"}, {"unblock"}, {"unblock", "a", "b"}} {
		if code, _, errs := run(bad[0], bad[1:]...); code != 1 || !strings.HasPrefix(errs, "error: ") {
			t.Errorf("%q: %d %q", bad, code, errs)
		}
	}
	if code, out, _ := run("unblock", "123456789"); code != 0 || strings.TrimSpace(out) != "unblocked: 123456789" {
		t.Fatalf("unblock: %d %q", code, out)
	}
	if code, _, errs := run("unblock", "123456789"); code != 1 || !strings.Contains(errs, "is not blocked") {
		t.Fatalf("unblock twice: %d %q", code, errs)
	}
	// The running server reads the same database.
	st, err := store.Open(filepath.Join(dir, "nuvira.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer st.Close()
	if blocks, err := st.Blocks(); err != nil || len(blocks) != 3 {
		t.Fatalf("blocks in the database: %v %v", blocks, err)
	}
}

func TestHelpAndUnknownCommand(t *testing.T) {
	t.Setenv("NUVIRA_DATA_DIR", tempDir(t))
	for _, name := range []string{"help", "-h", "--help"} {
		code, out, errs := capture(t, func() int { return command(name, nil) })
		if code != 0 || out != "" || !strings.Contains(errs, "healthcheck") || !strings.Contains(errs, "block ") ||
			!strings.Contains(errs, "("+edition+")") || strings.Contains(errs, "unknown command") {
			t.Errorf("%s: %d %q %q", name, code, out, errs)
		}
	}
	code, _, errs := capture(t, func() int { return command("frobnicate", []string{"x"}) })
	if code != 2 || !strings.Contains(errs, `unknown command "frobnicate"`) {
		t.Fatalf("unknown: %d %q", code, errs)
	}
	// Flags are not parsed: "-v" or "--version" is an unknown command too.
	if code, _, _ := capture(t, func() int { return command("--version", nil) }); code != 2 {
		t.Fatalf("--version: %d", code)
	}
}

// A command on a data folder that cannot hold the database: exit 1.
func TestCommandBadDataDir(t *testing.T) {
	file := filepath.Join(tempDir(t), "not-a-dir")
	if err := os.WriteFile(file, []byte("x"), 0o600); err != nil {
		t.Fatal(err)
	}
	t.Setenv("NUVIRA_DATA_DIR", file)
	code, _, errs := capture(t, func() int { return command("blocked", nil) })
	if code != 1 || !strings.HasPrefix(errs, "database:") && !strings.HasPrefix(errs, "error:") {
		t.Fatalf("%d %q", code, errs)
	}
}

func TestEnv(t *testing.T) {
	t.Setenv("NUVIRA_P2_X", "")
	if env("NUVIRA_P2_X", "def") != "def" {
		t.Fatal("empty value must give the default")
	}
	t.Setenv("NUVIRA_P2_X", "v")
	if env("NUVIRA_P2_X", "def") != "v" {
		t.Fatal("value")
	}
}

// healthcheck completes a QUIC handshake with 127.0.0.1 on the port of
// NUVIRA_LISTEN (any host part is ignored), certificate not checked, ALPN
// nuvira-signal/1; it fails within ~3 s when nobody answers.
func TestHealthcheck(t *testing.T) {
	cert, err := tlsutil.LoadOrCreate(tempDir(t))
	if err != nil {
		t.Fatal(err)
	}
	udp, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatal(err)
	}
	tr := &quic.Transport{Conn: udp}
	defer tr.Close()
	ln, err := tr.Listen(&tls.Config{Certificates: []tls.Certificate{cert}, NextProtos: []string{alpn}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()
	go func() {
		for {
			c, err := ln.Accept(context.Background())
			if err != nil {
				return
			}
			go func() { <-c.Context().Done() }()
		}
	}()
	port := udp.LocalAddr().(*net.UDPAddr).Port
	t.Setenv("NUVIRA_LISTEN", "203.0.113.1:"+strconv.Itoa(port))
	if err := healthcheck(); err != nil {
		t.Fatalf("healthy server: %v", err)
	}

	// A listener with another ALPN: refused.
	other, _ := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	tr2 := &quic.Transport{Conn: other}
	defer tr2.Close()
	ln2, err := tr2.Listen(&tls.Config{Certificates: []tls.Certificate{cert}, NextProtos: []string{"nuvira/0"}}, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer ln2.Close()
	t.Setenv("NUVIRA_LISTEN", ":"+strconv.Itoa(other.LocalAddr().(*net.UDPAddr).Port))
	if err := healthcheck(); err == nil {
		t.Fatal("session ALPN accepted by the healthcheck")
	}

	// Nobody on the port.
	free, _ := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	freePort := free.LocalAddr().(*net.UDPAddr).Port
	free.Close()
	t.Setenv("NUVIRA_LISTEN", ":"+strconv.Itoa(freePort))
	start := time.Now()
	if err := healthcheck(); err == nil {
		t.Fatal("healthcheck passed with no server")
	}
	if d := time.Since(start); d > 5*time.Second {
		t.Fatalf("took %v", d)
	}
	t.Setenv("NUVIRA_LISTEN", "no-port")
	if err := healthcheck(); err == nil {
		t.Fatal("address without a port accepted")
	}
}

func TestRateLimitMode(t *testing.T) {
	log := slog.New(slog.NewTextHandler(io.Discard, nil))
	for in, want := range map[string]limits.Mode{"": limits.Public, "off": limits.Off, "all": limits.All, "yes": limits.Public} {
		if got := rateLimitMode(in, log); got != want {
			t.Errorf("%q: %v", in, got)
		}
	}
}

// SRV-17: a keep-alive HTTP connection idle between requests is closed after
// the idle timeout (300 ms here), while a WebSocket tunnel (hijacked) on the
// same server stays open well past it.
func TestHTTPIdleTimeout(t *testing.T) {
	udp, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatal(err)
	}
	conn := relay.NewConn(udp, relay.New(slog.New(slog.NewTextHandler(io.Discard, nil))))
	defer conn.Close()
	mux := http.NewServeMux()
	mux.Handle("/nuvira", conn.WebSockets())
	mux.HandleFunc("/hello", func(w http.ResponseWriter, _ *http.Request) { _, _ = io.WriteString(w, "hi") })
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	const idle = 300 * time.Millisecond
	srv := newHTTPServer(ln.Addr().String(), mux, idle)
	go func() { _ = srv.Serve(ln) }()
	defer srv.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	ws, _, err := websocket.Dial(ctx, "ws://"+ln.Addr().String()+"/nuvira", nil)
	if err != nil {
		t.Fatal(err)
	}
	defer ws.CloseNow()

	c, err := net.Dial("tcp", ln.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer c.Close()
	if _, err := io.WriteString(c, "GET /hello HTTP/1.1\r\nHost: x\r\n\r\n"); err != nil {
		t.Fatal(err)
	}
	r := bufio.NewReader(c)
	resp, err := http.ReadResponse(r, nil)
	if err != nil {
		t.Fatal(err)
	}
	_, _ = io.Copy(io.Discard, resp.Body)
	resp.Body.Close()
	start := time.Now()
	_ = c.SetReadDeadline(time.Now().Add(5 * time.Second))
	if _, err := r.ReadByte(); err != io.EOF {
		t.Fatalf("idle keep-alive connection: %v", err)
	}
	if d := time.Since(start); d < idle/2 || d > 3*time.Second {
		t.Fatalf("closed after %v", d)
	}
	time.Sleep(3 * idle)
	if err := ws.Write(ctx, websocket.MessageBinary, []byte{0x40}); err != nil {
		t.Fatalf("WebSocket cut by the idle timeout: %v", err)
	}
	ws.CloseRead(ctx)
	if err := ws.Ping(ctx); err != nil {
		t.Fatalf("WebSocket after the idle timeout: %v", err)
	}
}
