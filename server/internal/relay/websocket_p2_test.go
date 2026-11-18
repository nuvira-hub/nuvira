package relay

// Phase 2 (characterisation): the WebSocket fallback over a real HTTP server.

import (
	"bytes"
	"context"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"
)

type fromPacket struct {
	data []byte
	from net.Addr
}

func pumpFrom(c *Conn, got chan<- fromPacket) {
	buf := make([]byte, 64<<10)
	for {
		n, from, err := c.ReadFrom(buf)
		if err != nil {
			return
		}
		got <- fromPacket{append([]byte(nil), buf[:n]...), from}
	}
}

func newWSServer(t *testing.T) (*Conn, *httptest.Server, chan fromPacket) {
	t.Helper()
	c, _ := newLoopbackConn(t)
	c.WebSockets().SetLogger(slog.New(slog.NewTextHandler(io.Discard, nil)))
	got := make(chan fromPacket, 64)
	go pumpFrom(c, got)
	srv := httptest.NewServer(c.WebSockets())
	t.Cleanup(srv.Close)
	return c, srv, got
}

func dialWS(t *testing.T, srv *httptest.Server, header http.Header) *websocket.Conn {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	ws, _, err := websocket.Dial(ctx, "ws"+strings.TrimPrefix(srv.URL, "http"), &websocket.DialOptions{HTTPHeader: header})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = ws.CloseNow() })
	return ws
}

// Binary messages are datagrams from a virtual address "ws:<n>@<ip>"; text and
// empty messages are ignored; the answer goes back on the same connection;
// from loopback the last X-Forwarded-For entry is the address.
func TestWebSocketDatagrams(t *testing.T) {
	c, srv, got := newWSServer(t)
	ws := dialWS(t, srv, http.Header{"X-Forwarded-For": {"198.51.100.1, 203.0.113.5"}})
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	must := func(err error) {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
	}
	must(ws.Write(ctx, websocket.MessageText, []byte("ignored")))
	must(ws.Write(ctx, websocket.MessageBinary, nil))
	must(ws.Write(ctx, websocket.MessageBinary, []byte{0x40, 1, 2}))
	var p fromPacket
	select {
	case p = <-got:
	case <-time.After(5 * time.Second):
		t.Fatal("datagram not delivered")
	}
	if !bytes.Equal(p.data, []byte{0x40, 1, 2}) {
		t.Fatalf("first datagram %x (text or empty message delivered?)", p.data)
	}
	if p.from.Network() != "ws" || !strings.HasPrefix(p.from.String(), "ws:") || !strings.HasSuffix(p.from.String(), "@203.0.113.5") {
		t.Fatalf("address %v", p.from)
	}
	if ip, ok := HostOf(p.from); !ok || ip.String() != "203.0.113.5" {
		t.Fatalf("HostOf %v %v", ip, ok)
	}
	if _, err := c.WriteTo([]byte{0x41, 9}, p.from); err != nil {
		t.Fatal(err)
	}
	typ, data, err := ws.Read(ctx)
	if err != nil || typ != websocket.MessageBinary || !bytes.Equal(data, []byte{0x41, 9}) {
		t.Fatalf("answer %v %x %v", typ, data, err)
	}
	// An unknown virtual address: closed.
	if _, err := c.WriteTo([]byte{1}, wsAddr("ws:999@1.2.3.4")); err != net.ErrClosed {
		t.Fatalf("unknown connection: %v", err)
	}
}

// A message over wsMaxMessage (2048) ends the connection (read limit); one of
// exactly 2048 bytes is delivered.
func TestWebSocketMessageLimit(t *testing.T) {
	_, srv, got := newWSServer(t)
	ws := dialWS(t, srv, nil)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := ws.Write(ctx, websocket.MessageBinary, bytes.Repeat([]byte{0x40}, wsMaxMessage)); err != nil {
		t.Fatal(err)
	}
	select {
	case p := <-got:
		if len(p.data) != wsMaxMessage {
			t.Fatalf("%d bytes", len(p.data))
		}
	case <-time.After(5 * time.Second):
		t.Fatal("2048-byte message not delivered")
	}
	_ = ws.Write(ctx, websocket.MessageBinary, bytes.Repeat([]byte{0x40}, wsMaxMessage+1))
	_, _, err := ws.Read(ctx)
	if websocket.CloseStatus(err) != websocket.StatusMessageTooBig {
		t.Fatalf("after a too large message: %v", err)
	}
	select {
	case p := <-got:
		t.Fatalf("too large message delivered (%d bytes)", len(p.data))
	case <-time.After(200 * time.Millisecond):
	}
}

// Characterisation: any Origin is accepted (InsecureSkipVerify, lead websocket.go:57)
// and there is no limit on the number of connections from one address.
func TestCharacterisesWebSocketAnyOriginManyConnections(t *testing.T) {
	_, srv, got := newWSServer(t)
	const n = 100
	conns := make([]*websocket.Conn, n)
	var wg sync.WaitGroup
	for i := range conns {
		wg.Add(1)
		go func() {
			defer wg.Done()
			conns[i] = dialWS(t, srv, http.Header{"Origin": {"https://evil.example"}})
		}()
	}
	wg.Wait()
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	for _, ws := range conns {
		if err := ws.Write(ctx, websocket.MessageBinary, []byte{0x40}); err != nil {
			t.Fatal(err)
		}
	}
	seen := map[string]bool{}
	for range n {
		select {
		case p := <-got:
			seen[p.from.String()] = true
		case <-time.After(5 * time.Second):
			t.Fatalf("only %d connections delivered", len(seen))
		}
	}
	if len(seen) != n {
		t.Fatalf("%d distinct addresses for %d connections", len(seen), n)
	}
}

// More remoteHost cases: only the last header line counts, link-local peers are
// not trusted as proxies, the header is ignored from public peers.
func TestRemoteHostMoreCases(t *testing.T) {
	for _, c := range []struct {
		remote string
		fwd    []string
		want   string
	}{
		{"127.0.0.1:1", []string{"1.1.1.1", "2.2.2.2, 3.3.3.3"}, "3.3.3.3"},
		{"127.0.0.1:1", []string{"1.1.1.1", "garbage"}, "127.0.0.1"}, // last line unreadable: the first is not used
		{"10.0.0.2:1", []string{"2001:db8::7"}, "2001:db8::7"},
		{"172.16.0.1:1", []string{" 4.4.4.4 "}, "4.4.4.4"},
		{"[fd00::1]:1", []string{"5.5.5.5"}, "5.5.5.5"},
		{"169.254.1.1:1", []string{"6.6.6.6"}, "169.254.1.1"}, // link-local: not a trusted proxy
		{"[fe80::1]:1", []string{"6.6.6.6"}, "fe80::1"},
		{"100.64.0.1:1", []string{"7.7.7.7"}, "100.64.0.1"}, // CGNAT is not private
		{"127.0.0.1:1", []string{"::ffff:8.8.8.8"}, "::ffff:8.8.8.8"},
		{"not-an-address", []string{"9.9.9.9"}, "not-an-address"},
	} {
		req := &http.Request{RemoteAddr: c.remote, Header: http.Header{"X-Forwarded-For": c.fwd}}
		if got := remoteHost(req); got != c.want {
			t.Errorf("%s + %q: %s, want %s", c.remote, c.fwd, got, c.want)
		}
	}
}
