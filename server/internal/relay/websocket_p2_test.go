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
	"net/netip"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/coder/websocket"

	"github.com/nuvira-hub/nuvira/server/internal/limits"
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

// A message over wsMaxMessage (the largest UDP payload, as over UDP: SRV-11)
// ends the connection (read limit); one of exactly wsMaxMessage bytes is delivered.
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
		t.Fatal("message of wsMaxMessage bytes not delivered")
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

// Characterisation (K22): any Origin is accepted (InsecureSkipVerify, kept on
// purpose: see ServeHTTP) and, from a local address with the default
// NUVIRA_RATE_LIMITS, there is no limit on the connections from one address.
// From public addresses (or any with "all") they are capped (SRV-14):
// TestWebSocketConnectionsPerAddressCapped.
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

// SRV-14: at most wsMaxPerAddress WebSocket connections at once per address
// (3 here, with "all" so that loopback counts); refused with 429, and a place
// is given back when a connection closes.
func TestWebSocketConnectionsPerAddressCapped(t *testing.T) {
	c, _ := newLoopbackConn(t)
	c.WebSockets().SetLogger(slog.New(slog.NewTextHandler(io.Discard, nil)))
	c.WebSockets().setLimits(limits.All, 3, wsDatagramsPerSecond)
	srv := httptest.NewServer(c.WebSockets())
	t.Cleanup(srv.Close)
	conns := []*websocket.Conn{dialWS(t, srv, nil), dialWS(t, srv, nil), dialWS(t, srv, nil)}
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	url := "ws" + strings.TrimPrefix(srv.URL, "http")
	_, resp, err := websocket.Dial(ctx, url, nil)
	if err == nil || resp == nil || resp.StatusCode != http.StatusTooManyRequests {
		t.Fatalf("4th connection: %v %v", resp, err)
	}
	_ = conns[0].Close(websocket.StatusNormalClosure, "")
	for range 100 {
		if c.WebSockets().open.Open(netip.MustParseAddr("127.0.0.1")) < 3 {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	dialWS(t, srv, nil)
	// Off: no cap.
	off, _ := newLoopbackConn(t)
	off.WebSockets().SetLimitMode(limits.Off)
	osrv := httptest.NewServer(off.WebSockets())
	t.Cleanup(osrv.Close)
	for range 5 {
		dialWS(t, osrv, nil)
	}
}

// SRV-14: one WebSocket connection sending as fast as it can no longer fills
// the queue shared with the UDP port: over wsDatagramsPerSecond (100/s here)
// its datagrams are dropped, and UDP datagrams sent meanwhile all arrive.
// Without the bucket its 6,000 datagrams fill the 4,096-slot queue and the
// UDP ones are dropped.
func TestWebSocketFloodDoesNotStarveUDP(t *testing.T) {
	c, _ := newLoopbackConn(t)
	c.WebSockets().SetLogger(slog.New(slog.NewTextHandler(io.Discard, nil)))
	c.WebSockets().setLimits(limits.All, wsMaxPerAddress, 100)
	srv := httptest.NewServer(c.WebSockets())
	t.Cleanup(srv.Close)
	ws := dialWS(t, srv, nil)
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	// Nobody reads the Conn yet: whatever is delivered stays in the queue.
	const flood = 6000
	for range flood {
		if err := ws.Write(ctx, websocket.MessageBinary, []byte{0x40, 0x01}); err != nil {
			t.Fatal(err)
		}
	}
	// The last WebSocket message read by the server: a ping round trip (the
	// pong needs a reader on this side).
	ctx = ws.CloseRead(ctx)
	if err := ws.Ping(ctx); err != nil {
		t.Fatal(err)
	}
	udp, err := net.DialUDP("udp", nil, c.udp.LocalAddr().(*net.UDPAddr))
	if err != nil {
		t.Fatal(err)
	}
	defer udp.Close()
	const honest = 50
	for range honest {
		if _, err := udp.Write([]byte{0x40, 0x02}); err != nil {
			t.Fatal(err)
		}
	}
	time.Sleep(300 * time.Millisecond)
	got := make(chan fromPacket, inQueue+honest)
	go pumpFrom(c, got)
	fromWS, fromUDP := 0, 0
	for done := false; !done; {
		select {
		case p := <-got:
			if p.data[1] == 0x01 {
				fromWS++
			} else {
				fromUDP++
			}
		case <-time.After(500 * time.Millisecond):
			done = true
		}
	}
	if fromUDP != honest || c.drops.Load() != 0 {
		t.Fatalf("UDP %d/%d delivered, %d dropped by the full queue (WebSocket %d of %d)", fromUDP, honest, c.drops.Load(), fromWS, flood)
	}
	if fromWS > 1000 {
		t.Fatalf("WebSocket delivered %d of %d at 100/s", fromWS, flood)
	}
}
